pub enum Command {
    Play,
    Pause,
    Stop,
    SetVolume(f32),
    NoteOn { note: u8, velocity: u8 },
    NoteOff { note: u8 },
    ConnectPlugin(plugin_host::instance::PluginInstance),
}
