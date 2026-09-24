# macOS VoiceProcessingIO regression and hardware validation

Tested on Apple Silicon, macOS 26.6.2, built-in microphone and speakers.
This is native hardware validation, not a software-AEC simulation or a VM.

## Reproduced failure

At e7edfa174466e87a467d1a58e3a4aec233ff8a21, the public dynamic `cpal::Device`
uses the trait default `build_input_stream`. That default drops the macOS
voice-processing argument and calls raw capture. Tracing
`AudioComponentInstanceNew` shows `ahal` despite requesting `screenpipe_aec()`.
Copying the new `live_dynamic_vpio_reaches_native_duplex_unit` test to that
revision fails reading the VPIO property with `AudioUnit(InvalidProperty)`.
Forwarding the argument alone then reaches `vpio` but fails stream creation.

The native setup also configured IO after coreaudio-rs had initialized the unit,
disabled its duplex output, and bound the microphone to the output-device bus.
The fixed path uninitializes before configuration, binds input and default output
to their respective buses, installs silent output plus the capture callback,
checks required voice properties, and initializes before starting.

```text
before: public Device + VPIO option -> trait default -> AUHAL (ahal)
after:  public Device + VPIO option -> CoreAudio override -> native VPIO (vpio)
                                      input bus 1 = microphone
                                      output bus 0 = default speaker
                                      render callback = silence
```

`MuteOutput` controls the processed microphone uplink, not speaker playback.
It stays zero. The render callback never feeds captured system audio back into
the speakers and adds no allocation or lock.

The topology follows Chromium's native implementation:
https://chromium.googlesource.com/chromium/src/media/+/05a7093e606816e8fd9c4db654554e96ec9790bb/audio/apple/audio_low_latency_input.cc

## Repeatable checks

```sh
cargo +1.94.0 test --lib
cargo +1.94.0 check --all-targets
# Opt-in real microphone access. Captured samples are counted, never stored.
cargo +1.94.0 test --lib live_ -- --ignored --test-threads=1
```

Results: two regular tests pass; two hardware tests pass. Hardware assertions
exercise the dynamic API at 16, 48, and 96 kHz, then 48 kHz with processing
bypassed. They verify input/output device bindings, enabled duplex output,
bypass and AGC readback, unmuted uplink, sustained capture, and zero callback
errors. A separate test verifies that omitting the option still selects AUHAL.
The regression test fails on the original revision and passes after the fix.

For bounded acoustic reproduction, the example records actual microphone WAVs:

```sh
cargo +1.94.0 run --release --example vpio_probe -- raw 18 /tmp/raw.wav
cargo +1.94.0 run --release --example vpio_probe -- bypass 18 /tmp/bypass.wav
cargo +1.94.0 run --release --example vpio_probe -- vpio 18 /tmp/vpio.wav
```

During each capture, play the same public speech recording through the speakers
at the same level, beginning two seconds after `READY`. The example does not
change volume or playback anything. It refuses to overwrite existing output.
The callback uses a preallocated bounded ring buffer; WAV writing happens only
after capture stops. Keep microphone recordings private.

## Acoustic observations and limits

Real public-speech playback was captured on this Mac through the original and
patched public CPAL APIs, with zero callback errors. Native-unit tracing changed
from `ahal` to `vpio`. Matched-speech leakage was lower in the patched capture,
but native VPIO can duck other applications' playback: the observed 25 dB
before/after difference is **not a clean AEC/ERLE measurement**.

A separate direct-AudioUnit experiment explicitly undid output ducking and
compared bypassed versus processed capture; matched-speech gain fell by about
11 dB in that pair. This is exploratory, from one Mac, not an acceptance
threshold or an acoustic-quality guarantee. The shipping patch does not use
that experimental ducking SPI. A simultaneous raw-mic control also changed with
VPIO active, so it cannot establish independent near-end preservation.

Not validated: human double-talk/near-end preservation, Bluetooth/USB/HDMI,
output-route changes during a stream, Intel, older macOS, or a long meeting.
No private customer recordings are included. These gaps matter because this fix
actually activates a native path that the public API previously skipped.
