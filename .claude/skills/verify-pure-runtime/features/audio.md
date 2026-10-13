# Audio

Built with the `audio` feature (`cargo build --release --features audio`, which needs FFmpeg's development files, found with `pkg-config`, and on Linux ALSA's, `libasound2-dev`), the runtime plays the game's sound. FFmpeg decodes the title's XMA streams, the title's own mixer submits frames of 256 six-channel samples, and the runtime downmixes them to stereo (leaving out the LFE channel) and plays them on the host's default output device at the console's 48 kHz. `PURE_AUDIO_CAPTURE=<file.wav>` also records what plays (48 kHz stereo, 16-bit), so a run can be judged without listening. Without the feature the title still submits its frames, and they are dropped: the game runs silent.

## Sub-features

- `audio-submit` the title submits mixed frames (`audio: first audio frame submitted`, with or without the feature).
- `audio-output` plays them on the default output device (`audio: playing through the default output device at 48000 Hz`, or a `WARN audio: no audio output: …` line).
- `audio-capture` records them to the WAV file `PURE_AUDIO_CAPTURE` names (`audio: capturing audio to <path>`).
- `audio-xma` decodes the title's XMA streams with FFmpeg (`xma: N passes decoded M frames in the last 10s` every 10 seconds; `WARN xma: FFmpeg could not open a WMA Pro decoder; XMA audio stays silent` when FFmpeg lacks the decoder).

## How to get to it (user POV)

- Run a build with `--features audio` and play: the start-up screens, menus and tutorial have music and effects.

## Driving it with run.sh

Preconditions:

- `scripts/doctor.sh` prints `ready` and `PURE_RUNTIME_BINARY` points at a build with `--features audio`. A run plays sound through the host's speakers.
- Python 3 with NumPy for the analysis below.

- **Capture.** Add `PURE_AUDIO_CAPTURE=/tmp/pure-audio.wav` to the tutorial run in [rendering.md](./rendering.md) (or any run that gets past the start-up screens). `runtime.log` shows `audio: first audio frame submitted`, `audio: capturing audio to /tmp/pure-audio.wav`, `audio: playing through the default output device at 48000 Hz`, and `xma: … passes decoded … frames` lines every 10 seconds.
- **Judge it.** Read the WAV with Python's `wave` module, convert its little-endian 16-bit samples to floats in -1 to 1 (`numpy.frombuffer(…, dtype='<i2') / 32768`), average the channels, and per second compute the RMS and, for seconds with RMS above 0.01, the spectral flatness (geometric over arithmetic mean of the Hann-windowed power spectrum). Real music and effects stay under about 0.3; decoding gone wrong is noise near 0.5 to 0.9 with energy around 10 kHz. On an M4 the tutorial run is audible for 360 of its 399 seconds (median RMS 0.15) with a median flatness of 0.010 and a maximum of 0.066. Keep the numbers in the evidence directory and delete the WAV: it is the game's sound.
- **Stereo streams.** `VERIFY_RUST_LOG=info,xma=debug` logs `a stream its context calls 1-channel decodes as 2-channel` as such streams are found.

## Gotchas

- Each playing 48 kHz stream needs about 94 XMA frames a second, and the title decodes when its own audio thread asks, so the `xma:` totals follow how fast the game runs: a loaded host decodes fewer.
- The WAV and anything derived from it that could be played back are game-derived: never commit or upload them.
