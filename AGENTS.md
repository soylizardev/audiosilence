# audiosilence — Agent Guide

A Rust-based DAW (Digital Audio Workstation) with Slint UI, cpal audio, CLAP plugin hosting, MIDI input, and WAV recording.

## Essential Commands

```bash
cargo build           # Build the project
cargo run             # Build and run
cargo build --release # Release build (optimized)
```

No tests, lints, or CI config exist. `cargo test` yields nothing. The project runs a real-time audio engine — expect audio device dependencies.

## Code Organization

```
├── build.rs                  # Slint UI compilation (slint_build::compile)
├── Cargo.toml                # Dependencies: slint, cpal, ringbuf, midir, tokio, hound, plugin_host
├── ui/appwindow.slint        # Slint UI: timeline, mixer window, transport, recording controls
└── src/
    ├── main.rs               # Entry point: audio callback, MIDI, recording thread, UI wiring
    ├── audio_track.rs        # AudioTrack: sample playback (Vec<f32>, one-shot)
    ├── command.rs            # Command enum: IPC between UI/MIDI threads and audio callback
    ├── oscillator.rs         # Voice: sine wave oscillator with simple on/off envelope
    ├── plugin_loader.rs      # CLAP plugin loading via plugin_host crate
    ├── sample_loader.rs      # WAV file loader (f32, i16, i24, i32; mono mixdown)
    └── transport.rs          # Transport: BPM, sample counter, samples-per-beat calc
```

## Architecture & Control Flow

```
UI (Slint callbacks) ──┐
                        ├──→ [Command ring buffer] ──→ Audio Callback
MIDI Input (midir) ────┘                                │
                                                        ├─ Voice (16-poly sine)
                                                        ├─ AudioTrack (sample playback)
                                                        ├─ Plugin (CLAP processing)
                                                        └─ Transport advance
```

- **Two lock-free ring buffers** (`ringbuf::SharedRb<Heap<Command>>`, size 256): one for UI commands, one for MIDI. Both use the same `Command` enum.
- **Audio callback** (`cpal::build_output_stream`) is the real-time heartbeat. It pops all pending commands from both buffers, then renders audio (voices → track → gain → optional CLAP plugin).
- **Recording** uses a separate ring buffer (`SharedRb<Heap<f32>>`, size 65536) and a disk writer thread that polls every 10ms.
- **Slint UI** is compiled at build time. `slint::include_modules!()` generates Rust bindings for all `export component` declarations in the `.slint` file.
- **MixerWindow** is a separate Slinnit window, toggled via a `Cell<bool>` flag and `show()`/`hide()`.

## Key Non-Obvious Patterns

### Command enum is the sole IPC mechanism
UI callbacks and MIDI callbacks both push `Command` variants into their respective ring buffers. The audio callback processes both in sequence — UI commands first, then MIDI. MIDI only handles `NoteOn`/`NoteOff`; all other variants are ignored from the MIDI buffer.

### Audio callback runs SCHED_FIFO (Linux)
`try_set_audio_thread_priority()` uses `libc::pthread_setschedparam` with priority 80 and `SCHED_FIFO`. This requires `CAP_SYS_NICE` or root. Failure is non-fatal (prints warning). Only applies on `cfg!(target_os = "linux")`.

### Pause advances voices (prevents hanging notes)
When `master_mute` is true, the callback fills output with zeros BUT still calls `voice.next_sample()` and `track.next_sample()` for every frame. Without this, voices would never reach their release phase and would hang indefinitely at note-off.

### 16-voice polyphony with naive allocation
`voices: [Voice; 16]` uses first-free-voice allocation (`!voice.is_active()`). When all 16 are active, new `NoteOn` is silently dropped. Voice stealing is not implemented.

### SIMD mixer with 8-track support (wide crate)
The audio callback mixes up to 8 concurrent `AudioTrack` instances using `f32x4` SIMD vectors from the `wide` crate. The render loop processes frames in blocks of 4 using `f32x4::new(...)`/`to_array()`, with a scalar fallback for remainders when `frames % 4 != 0`. This is lock-free and allocation-free. Tracks are stored in `[AudioTrack; 8]` and `PlaySample` finds the first inactive track or overwrites track 0.

### Voice envelope is not ADSR
Just a flat `env_gain = 1.0` on note-on, then a linear decay at rate `0.001` per sample on note-off. No attack, decay, sustain, or release stages beyond this simple ramp.

### Plugin processing runs inside the audio callback
CLAP plugin `process()` is called synchronously in the audio render loop, using scratch buffers (`scratch_left/right`, `MAX_BUF_FRAMES = 8192`). A slow plugin will cause audio x-runs (glitches). The plugin path is hardcoded: `/usr/lib/clap/DuskVerb.clap`; failure is expected and non-fatal.

### WAV file loading is format-aware
Via `sample_loader::load_wav_file`: handles f32, i16, i24, i32 formats. Multichannel WAVs are mixed down to mono (averaged). The i24 path uses a manual shift (`>> 8`) rather than the hound-native approach. Runs in `tokio::task::spawn_blocking`.

### Hardcoded paths
- **Plugin**: `/usr/lib/clap/DuskVerb.clap` — loaded at startup, expects failure silently.
- **Recording output**: `recording_{unix_timestamp}.wav` in CWD.

### Recording is single-threaded, polling
A background thread (`std::thread::spawn`) polls the recording ring buffer every 10ms and writes to a `hound::WavWriter`. Uses `AtomicBool` flags (`is_recording`, `disk_running`) with `Ordering::Relaxed`/`Ordering::SeqCst`. Recording pauses/resumes the input stream via `cpal::Stream::pause()`/`play()`.

### MIDI auto-connects to first non-"Midi Through" port
Skips ports whose name contains "Midi Through". If no suitable port found, falls back to the first port. If no ports at all, logs warning and continues without MIDI.

### Slint module inclusion
The `.slint` file uses `export component AppWindow` and `export component MixerWindow`. `slint::include_modules!()` in main.rs generates `AppWindow` and `MixerWindow` Rust types. All UI callbacks are wired after `AppWindow::new()`/`MixerWindow::new()`.

### Playhead position is computed from sample position
A 16ms `slint::Timer` polls `AtomicU64` sample position and converts to pixels: `beats = sample_pos / (sample_rate * 60 / 120)` × 80px per beat. The result is written to `playhead-position` on the AppWindow.

### Transport controls: Play/Pause/Stop
Three separate UI callbacks — `play`, `pause`, `stop` — each push the corresponding `Command` variant. `Stop` resets `transport.current_sample` to 0, reinitializes all voices, clears all audio tracks, and rewinds the playhead to 0 pixels.

### Track rows are 50px fixed height
The `TrackHeader` (left panel) and `TimelineTrack` (grid) both use `height: 50px` with `spacing: 0` in their vertical containers, ensuring 1:1 vertical alignment. The ruler is 24px.

### Mixer generates 9 channel strips dynamically
The `MixerWindow` uses `for channelIdx in 9: ChannelStrip` — channels 0-7 map to tracks 1-8, channel 8 is "Master". The old 3 hardcoded strips are removed.

### File Browser placeholder
A 200px `Rectangle` sits on the right of the workspace with a "Coming soon..." placeholder, reserved for future file explorer integration. The timeline grid's `ScrollView` ensures horizontal scrolling for 256 beats of content.
