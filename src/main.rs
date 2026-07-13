mod audio_track;
mod command;
mod oscillator;
mod plugin_loader;
mod transport;

use audio_track::AudioTrack;
use command::Command;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use oscillator::Voice;
use ringbuf::{storage::Heap, traits::*, SharedRb};
use wide::f32x4;
use std::cell::Cell;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use transport::Transport;
use midir::{Ignore, MidiInput, MidiInputConnection};

slint::include_modules!();

fn try_set_audio_thread_priority() {
    static SET: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    SET.get_or_init(|| {
        #[cfg(target_os = "linux")]
        unsafe {
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = 80;
            let ret = libc::pthread_setschedparam(
                libc::pthread_self(),
                libc::SCHED_FIFO,
                &param,
            );
            if ret != 0 {
                eprintln!(
                    "warning: could not set audio thread priority \
                     (need CAP_SYS_NICE or root): {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("no default output device found");

    let config = device.default_output_config()?;
    let sample_rate = config.sample_rate() as f64;
    println!("device: {}", device.description()?.name());
    println!(
        "format: {:?}, rate: {} Hz, channels: {}",
        config.sample_format(),
        sample_rate,
        config.channels()
    );

    if config.sample_format() != cpal::SampleFormat::F32 {
        return Err("only f32 sample format is supported".into());
    }

    let rb = SharedRb::<Heap<Command>>::new(256);
    let (tx, mut rx) = rb.split();
    let tx = Arc::new(Mutex::new(tx));

    match plugin_loader::load_plugin("/usr/lib/clap/DuskVerb.clap", 48000.0) {
        Ok(inst) => {
            println!(
                "[main] Plugin loaded: \"{}\" by {} (v{})",
                inst.descriptor.name,
                inst.descriptor.vendor,
                inst.descriptor.version
            );
            let _ = tx.lock().unwrap().try_push(Command::ConnectPlugin(inst));
        }
        Err(e) => {
            eprintln!("[main] Plugin load test (expected if no .clap file): {}", e);
        }
    }

    let midi_rb = SharedRb::<Heap<Command>>::new(256);
    let (mut midi_tx, mut midi_rx) = midi_rb.split();

    const BEAT_ACTIVE_SAMPLES: u64 = 2000;

    let mut voices: [Voice; 16] = std::array::from_fn(|_| Voice::new(sample_rate));
    let mut transport = Transport::new(sample_rate);
    let mut tracks: [AudioTrack; 8] = std::array::from_fn(|_| AudioTrack::new(Vec::new()));
    let mut master_gain = 0.2;
    let mut master_mute = false;
    let mut active_plugin: Option<plugin_host::instance::PluginInstance> = None;
    const MAX_BUF_FRAMES: usize = 8192;
    let mut scratch_left: Vec<f32> = vec![0.0; MAX_BUF_FRAMES];
    let mut scratch_right: Vec<f32> = vec![0.0; MAX_BUF_FRAMES];

    let mut plugin_input_buf =
        plugin_host::process::PluginAudioBuffer::new(2, MAX_BUF_FRAMES);
    let mut plugin_output_buf =
        plugin_host::process::PluginAudioBuffer::new(2, MAX_BUF_FRAMES);
    let sample_pos = Arc::new(AtomicU64::new(0));
    let sample_pos_cb = sample_pos.clone();
    let stream_config = config.config();
    let stream = device.build_output_stream(
        stream_config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            try_set_audio_thread_priority();

            while let Some(cmd) = rx.try_pop() {
                match cmd {
                    Command::Play => master_mute = false,
                    Command::Pause => master_mute = true,
                    Command::Stop => {
                        master_mute = true;
                        transport.reset();
                    }
                    Command::SetVolume(gain) => master_gain = gain as f64,
                    Command::NoteOn { note, .. } => {
                        let freq =
                            440.0 * 2.0_f64.powf((note as f64 - 69.0) / 12.0);
                        for voice in &mut voices {
                            if !voice.is_active() {
                                voice.note_on(note, freq);
                                break;
                            }
                        }
                    }
                    Command::NoteOff { note } => {
                        for voice in &mut voices {
                            if voice.is_active() && voice.note() == note {
                                voice.note_off();
                                break;
                            }
                        }
                    }
                    Command::ConnectPlugin(processor) => {
                        active_plugin = Some(processor);
                    }
                }
            }

            while let Some(cmd) = midi_rx.try_pop() {
                match cmd {
                    Command::NoteOn { note, .. } => {
                        let freq =
                            440.0 * 2.0_f64.powf((note as f64 - 69.0) / 12.0);
                        for voice in &mut voices {
                            if !voice.is_active() {
                                voice.note_on(note, freq);
                                break;
                            }
                        }
                    }
                    Command::NoteOff { note } => {
                        for voice in &mut voices {
                            if voice.is_active() && voice.note() == note {
                                voice.note_off();
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }

            if master_mute {
                for _ in 0..data.len() / 2 {
                    for voice in &mut voices {
                        voice.next_sample();
                    }
                    for track in &mut tracks {
                        track.next_sample();
                    }
                }
                data.fill(0.0);
            } else {
                const LANES: usize = 4;
                let total_frames = data.len() / 2;
                let mut frame_offset = 0;

                // SIMD vector path: process LANES frames per iteration
                let simd_end = total_frames / LANES * LANES;
                while frame_offset < simd_end {
                    let mut sum = f32x4::splat(0.0);

                    for voice in &mut voices {
                        let mut block = [0.0f32; LANES];
                        for i in 0..LANES {
                            block[i] = voice.next_sample();
                        }
                        sum += f32x4::new(block);
                    }
                    for track in &mut tracks {
                        let mut block = [0.0f32; LANES];
                        for i in 0..LANES {
                            block[i] = track.next_sample();
                        }
                        sum += f32x4::new(block);
                    }

                    sum *= f32x4::splat(master_gain as f32);
                    let out = sum.to_array();

                    for (i, &s) in out.iter().enumerate() {
                        let idx = (frame_offset + i) * 2;
                        data[idx] = s;
                        data[idx + 1] = s;
                    }
                    frame_offset += LANES;
                }

                // Scalar remainder (when total_frames not divisible by LANES)
                while frame_offset < total_frames {
                    let mut s = 0.0;
                    for voice in &mut voices {
                        s += voice.next_sample();
                    }
                    for track in &mut tracks {
                        s += track.next_sample();
                    }
                    s *= master_gain as f32;
                    let idx = frame_offset * 2;
                    data[idx] = s;
                    data[idx + 1] = s;
                    frame_offset += 1;
                }

                let any_voice_active = voices.iter().any(|v| v.is_active());
                let any_track_active = tracks.iter().any(|t| t.is_active());
                if !any_voice_active && !any_track_active {
                    let spb = transport.samples_per_beat();
                    for (i, frame) in data.chunks_exact_mut(2).enumerate() {
                        let sample_pos = transport.current_sample + (i * 2) as u64;
                        if sample_pos % spb >= BEAT_ACTIVE_SAMPLES {
                            frame[0] = 0.0;
                            frame[1] = 0.0;
                        }
                    }
                }

                if let Some(ref mut plugin) = active_plugin {
                    let frames = data.len() / 2;
                    if frames <= MAX_BUF_FRAMES {
                        for i in 0..frames {
                            scratch_left[i] = data[i * 2];
                            scratch_right[i] = data[i * 2 + 1];
                        }

                        {
                            let ch = &mut plugin_input_buf.channels;
                            let (in_l, in_r) = ch.split_at_mut(1);
                            in_l[0][..frames].copy_from_slice(&scratch_left[..frames]);
                            in_r[0][..frames].copy_from_slice(&scratch_right[..frames]);
                            plugin_input_buf.frames = frames;

                            let ch = &mut plugin_output_buf.channels;
                            let (out_l, out_r) = ch.split_at_mut(1);
                            out_l[0][..frames].copy_from_slice(&scratch_left[..frames]);
                            out_r[0][..frames].copy_from_slice(&scratch_right[..frames]);
                            plugin_output_buf.frames = frames;
                        }

                        let mut process_data = plugin_host::process::PluginProcessData {
                            inputs: &plugin_input_buf,
                            outputs: &mut plugin_output_buf,
                            midi_events: &[],
                            param_events: &[],
                            sample_rate,
                            block_size: frames,
                            transport: plugin_host::process::PluginTransportInfo::default(),
                        };

                        let _ = plugin.process(&mut process_data);

                        {
                            let ch = &plugin_output_buf.channels;
                            let (out_l, out_r) = ch.split_at(1);
                            for i in 0..frames {
                                data[i * 2] = out_l[0][i];
                                data[i * 2 + 1] = out_r[0][i];
                            }
                        }
                    }
                }

                transport.advance(data.len());
            }
            sample_pos_cb.store(transport.current_sample, Ordering::Relaxed);
        },
        |err| eprintln!("audio stream error: {}", err),
        None,
    )?;

    stream.play()?;

    let _midi_conn: Option<MidiInputConnection<()>> =
        match MidiInput::new("audiosilence-input") {
            Ok(mut midi_in) => {
                midi_in.ignore(Ignore::None);
                let ports = midi_in.ports();
                if ports.is_empty() {
                    eprintln!("warning: no MIDI input ports found");
                    None
                } else {
                    println!("found {} MIDI input port(s):", ports.len());
                    for p in &ports {
                        if let Ok(name) = midi_in.port_name(p) {
                            println!("  {}", name);
                        }
                    }
                    let port = ports
                        .iter()
                        .find(|p| {
                            midi_in
                                .port_name(p)
                                .map_or(false, |n| !n.contains("Midi Through"))
                        })
                        .unwrap_or_else(|| ports.first().unwrap());
                    match midi_in.port_name(port) {
                        Ok(name) => println!("connecting to: {}", name),
                        Err(_) => eprintln!("warning: could not read MIDI port name"),
                    }
                    match midi_in.connect(
                        port,
                        "audiosilence-cb",
                        move |_stamp: u64, message: &[u8], _data: &mut ()| {
                            if message.len() < 3 {
                                return;
                            }
                            let status = message[0];
                            let note = message[1];
                            let velocity = message[2];
                            match status {
                                0x90 if velocity > 0 => {
                                    let _ = midi_tx
                                        .try_push(Command::NoteOn { note, velocity });
                                }
                                0x80 | 0x90 => {
                                    let _ =
                                        midi_tx.try_push(Command::NoteOff { note });
                                }
                                _ => {}
                            }
                        },
                        (),
                    ) {
                        Ok(conn) => Some(conn),
                        Err(e) => {
                            eprintln!(
                                "warning: could not connect to MIDI port: {}",
                                e
                            );
                            None
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("warning: could not create MIDI input: {}", e);
                None
            }
        };

    // === Recording Setup ===
    let record_rb = SharedRb::<Heap<f32>>::new(65536);
    let (mut record_tx, mut record_rx) = record_rb.split();
    let is_recording = Arc::new(AtomicBool::new(false));

    let input_stream: Option<Arc<Mutex<cpal::Stream>>> =
        match host.default_input_device() {
            Some(dev) => match dev.default_input_config() {
                Ok(cfg) if cfg.sample_format() == cpal::SampleFormat::F32 => {
                    let is_rec = is_recording.clone();
                    let input_channels: u16 = cfg.channels();
                    match dev.build_input_stream(
                        cfg.config(),
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            if is_rec.load(Ordering::Relaxed) {
                                if input_channels >= 2 {
                                    for pair in data.chunks_exact(2) {
                                        let mono = (pair[0] + pair[1]) * 0.5;
                                        let _ = record_tx.try_push(mono);
                                    }
                                } else {
                                    for &s in data {
                                        let _ = record_tx.try_push(s);
                                    }
                                }
                            }
                        },
                        |err| eprintln!("input stream error: {}", err),
                        None,
                    ) {
                        Ok(stream) => {
                            let _ = stream.pause();
                            println!(
                                "input device: {} ({} Hz, {} ch)",
                                dev.description()
                                    .map(|d| d.name().to_string())
                                    .unwrap_or_else(|_| "?".into()),
                                cfg.sample_rate(),
                                cfg.channels(),
                            );
                            Some(Arc::new(Mutex::new(stream)))
                        }
                        Err(e) => {
                            eprintln!("warning: could not build input stream: {}", e);
                            None
                        }
                    }
                }
                Ok(cfg) => {
                    eprintln!(
                        "warning: input format {:?} not supported (need f32)",
                        cfg.sample_format()
                    );
                    None
                }
                Err(e) => {
                    eprintln!("warning: could not read input config: {}", e);
                    None
                }
            },
            None => {
                eprintln!("warning: no default input device found");
                None
            }
        };

    let record_spec = hound::WavSpec {
        channels: 1,
        sample_rate: sample_rate as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };

    let disk_running = Arc::new(AtomicBool::new(true));
    let is_rec_disk = is_recording.clone();
    let run_disk = disk_running.clone();

    std::thread::spawn(move || {
        let mut writer: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>> =
            None;
        let mut was_recording = false;

        while run_disk.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(10));

            let now_recording = is_rec_disk.load(Ordering::Relaxed);

            if now_recording && !was_recording {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let filename = format!("recording_{}.wav", ts);
                let file = std::fs::File::create(&filename)
                    .expect("cannot create recording file");
                writer = Some(
                    hound::WavWriter::new(
                        std::io::BufWriter::new(file),
                        record_spec,
                    )
                    .expect("invalid WAV spec"),
                );
                println!("Recording -> {}", filename);
            }

            if let Some(ref mut w) = writer {
                while let Some(s) = record_rx.try_pop() {
                    let _ = w.write_sample(s);
                }
            }

            if !now_recording && was_recording {
                if let Some(w) = writer.take() {
                    let _ = w.finalize();
                    println!("Recording saved.");
                }
            }

            was_recording = now_recording;
        }

        if let Some(mut w) = writer.take() {
            while let Some(s) = record_rx.try_pop() {
                let _ = w.write_sample(s);
            }
            let _ = w.finalize();
        }
    });

    let ui = AppWindow::new()?;
    let mixer_ui = MixerWindow::new()?;
    ui.set_playing(true);

    let ui_weak = ui.as_weak();

    {
        let tx = tx.clone();
        let ui_weak = ui_weak.clone();
        ui.on_play(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = tx.lock().unwrap().try_push(Command::Play);
                ui.set_playing(true);
            }
        });
    }

    {
        let tx = tx.clone();
        let ui_weak = ui_weak.clone();
        ui.on_pause(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = tx.lock().unwrap().try_push(Command::Pause);
                ui.set_playing(false);
            }
        });
    }

    {
        let tx = tx.clone();
        let ui_weak = ui_weak.clone();
        ui.on_stop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = tx.lock().unwrap().try_push(Command::Stop);
                ui.set_playing(false);
                ui.set_playhead_position(0.0);
            }
        });
    }

    {
        let mixer_ui = mixer_ui.as_weak();
        let mixer_showing = Cell::new(false);
        ui.on_toggle_mixer(move || {
            if let Some(mixer) = mixer_ui.upgrade() {
                mixer_showing.set(!mixer_showing.get());
                if mixer_showing.get() {
                    let _ = mixer.show();
                } else {
                    let _ = mixer.hide();
                }
            }
        });
    }

    {
        let tx = tx.clone();
        mixer_ui.on_channel_volume_changed(move |_channel, volume| {
            let _ = tx.lock().unwrap().try_push(Command::SetVolume(volume));
        });
    }

    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // Recording UI callbacks
    {
        let is_recording = is_recording.clone();
        let input_stream = input_stream.clone();
        let ui_weak = ui.as_weak();
        ui.on_start_recording(move || {
            if let Some(ref stream) = input_stream {
                is_recording.store(true, Ordering::SeqCst);
                let _ = stream.lock().unwrap().play();
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_recording(true);
                }
                println!("Grabación iniciada");
            } else {
                eprintln!("No hay dispositivo de entrada disponible");
            }
        });
    }

    {
        let is_recording = is_recording.clone();
        let input_stream = input_stream.clone();
        let ui_weak = ui.as_weak();
        ui.on_stop_recording(move || {
            if let Some(ref stream) = input_stream {
                is_recording.store(false, Ordering::SeqCst);
                let _ = stream.lock().unwrap().pause();
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_recording(false);
                }
                println!("Grabación detenida — escribiendo archivo...");
            }
        });
    }

    let ui_weak = ui.as_weak();
    let sample_pos_timer = sample_pos.clone();
    let _playhead_timer = {
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(16),
            move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let samples = sample_pos_timer.load(Ordering::Relaxed);
                    let beats = (samples as f64) / (sample_rate * 60.0 / 120.0);
                    let px = (beats * 80.0) as f32;
                    ui.set_playhead_position(px);
                }
            },
        );
        timer
    };

    ui.run()?;

    disk_running.store(false, Ordering::Relaxed);

    Ok(())
}
