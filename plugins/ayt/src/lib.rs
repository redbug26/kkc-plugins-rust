use abi_stable::{
    export_root_module,
    prefix_type::PrefixTypeTrait,
    std_types::{RResult, RStr, RString, RVec},
};
use kkc_plugin_api::{
    AudioPcmChunk, AudioPlaybackSnapshot, AudioPluginMetadata, AudioPluginMod, AudioPluginModRef,
    AudioPluginResult, AudioTrackInfo, KKC_AUDIO_PLUGIN_API_VERSION,
};
use std::array;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

const HEADER_LEN: usize = 14;
const AY_REG_COUNT: usize = 14;
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const SPECTRUM_BANDS: usize = 96;

const AY_DAC: [f32; 32] = [
    0.0000000, 0.0000000, 0.0099947, 0.0099947, 0.0144503, 0.0144503, 0.0210575, 0.0210575,
    0.0307012, 0.0307012, 0.0455482, 0.0455482, 0.0644999, 0.0644999, 0.1073625, 0.1073625,
    0.1265888, 0.1265888, 0.2049897, 0.2049897, 0.2922103, 0.2922103, 0.3728389, 0.3728389,
    0.4925307, 0.4925307, 0.6353246, 0.6353246, 0.8055848, 0.8055848, 1.0000000, 1.0000000,
];

#[derive(Clone)]
struct AytSong {
    version: u8,
    active_mask: u16,
    active_registers: Vec<u8>,
    pattern_size: usize,
    frame_count: usize,
    sequence_count: usize,
    loop_frame: usize,
    frame_rate: u32,
    platform_id: u8,
    platform_name: &'static str,
    frequency_code: u8,
    master_clock_hz: f64,
    reg_frames: [Vec<u8>; AY_REG_COUNT],
    init_values: Vec<(u8, u8)>,
}

impl AytSong {
    fn duration_secs(&self) -> f64 {
        if self.frame_rate == 0 {
            0.0
        } else {
            self.frame_count as f64 / self.frame_rate as f64
        }
    }

    fn table_index_for_frame(&self, frame: usize) -> usize {
        if self.pattern_size == 0 {
            0
        } else {
            frame / self.pattern_size
        }
    }

    fn row_for_frame(&self, frame: usize) -> usize {
        if self.pattern_size == 0 {
            0
        } else {
            frame % self.pattern_size
        }
    }
}

#[derive(Clone)]
struct AyChip {
    regs: [u8; 16],
    tone_period: [i32; 3],
    tone_cnt: [i32; 3],
    tone_out: [i32; 3],
    noise_period: i32,
    noise_cnt: i32,
    noise_lfsr: u32,
    noise_out: i32,
    env_period: i32,
    env_cnt: i32,
    env_shape: i32,
    env_level: i32,
    env_hold: bool,
    env_alt: bool,
    env_atk: bool,
    env_cont: bool,
}

impl AyChip {
    fn new() -> Self {
        let mut chip = Self {
            regs: [0; 16],
            tone_period: [1; 3],
            tone_cnt: [0; 3],
            tone_out: [0; 3],
            noise_period: 1,
            noise_cnt: 0,
            noise_lfsr: 0x1FFFF,
            noise_out: 1,
            env_period: 1,
            env_cnt: 0,
            env_shape: 0,
            env_level: 0,
            env_hold: false,
            env_alt: false,
            env_atk: false,
            env_cont: false,
        };
        chip.env_reload();
        chip
    }

    fn write_reg(&mut self, index: usize, value: u8) {
        let idx = index & 0x0F;
        self.regs[idx] = value;
        match idx {
            0..=5 => self.recalc_tone_periods(),
            6 => self.recalc_noise_period(),
            0x0B | 0x0C => self.recalc_env_period(),
            0x0D => self.env_reload(),
            _ => {}
        }
    }

    fn recalc_tone_periods(&mut self) {
        self.tone_period[0] = ((self.regs[0] as i32) | (((self.regs[1] & 0x0F) as i32) << 8)).max(1);
        self.tone_period[1] = ((self.regs[2] as i32) | (((self.regs[3] & 0x0F) as i32) << 8)).max(1);
        self.tone_period[2] = ((self.regs[4] as i32) | (((self.regs[5] & 0x0F) as i32) << 8)).max(1);
    }

    fn recalc_noise_period(&mut self) {
        self.noise_period = ((self.regs[6] & 0x1F) as i32).max(1);
    }

    fn recalc_env_period(&mut self) {
        self.env_period = ((self.regs[0x0B] as i32) | ((self.regs[0x0C] as i32) << 8)).max(1);
    }

    fn env_reload(&mut self) {
        self.env_shape = (self.regs[0x0D] & 0x0F) as i32;
        self.env_cont = (self.env_shape & 0x08) != 0;
        self.env_atk = (self.env_shape & 0x04) != 0;
        self.env_alt = (self.env_shape & 0x02) != 0;
        self.env_hold = (self.env_shape & 0x01) != 0;
        self.env_level = if self.env_atk { 0 } else { 31 };
    }

    fn env_step(&mut self) {
        if self.env_atk {
            if self.env_level < 31 {
                self.env_level += 1;
            }
        } else if self.env_level > 0 {
            self.env_level -= 1;
        }

        let hit_top = self.env_atk && self.env_level == 31;
        let hit_bottom = !self.env_atk && self.env_level == 0;
        if !(hit_top || hit_bottom) {
            return;
        }

        if !self.env_cont {
            self.env_level = if self.env_atk { 31 } else { 0 };
            return;
        }
        if self.env_hold {
            return;
        }
        if self.env_alt {
            self.env_atk = !self.env_atk;
        } else {
            self.env_level = if self.env_atk { 0 } else { 31 };
        }
    }

    fn tick(&mut self) {
        for i in 0..3 {
            if self.tone_cnt[i] == 0 {
                self.tone_cnt[i] = self.tone_period[i];
            }
        }

        for i in 0..3 {
            self.tone_cnt[i] -= 1;
            if self.tone_cnt[i] == 0 {
                self.tone_out[i] ^= 1;
            }
        }

        if self.noise_cnt == 0 {
            self.noise_cnt = self.noise_period;
        }
        self.noise_cnt -= 1;
        if self.noise_cnt == 0 {
            if ((self.noise_lfsr ^ (self.noise_lfsr >> 3)) & 1) != 0 {
                self.noise_lfsr = (self.noise_lfsr >> 1) | 0x10000;
            } else {
                self.noise_lfsr >>= 1;
            }
            self.noise_out = (self.noise_lfsr & 1) as i32;
        }

        if self.env_cnt == 0 {
            self.env_cnt = self.env_period;
        }
        self.env_cnt -= 1;
        if self.env_cnt == 0 {
            self.env_step();
        }
    }

    fn mix_sample(&self) -> (f32, f32) {
        let mut mix_l = 0.0f32;
        let mut mix_r = 0.0f32;

        for ch in 0..3 {
            let gate_tone = if ((self.regs[7] >> ch) & 1) == 0 { 1 } else { 0 };
            let gate_noise = if ((self.regs[7] >> (ch + 3)) & 1) == 0 { 1 } else { 0 };

            let mut chan_out = 1;
            if gate_tone == 1 {
                chan_out &= self.tone_out[ch];
            }
            if gate_noise == 1 {
                chan_out &= 1 - self.noise_out;
            }

            let vol_idx = if (self.regs[8 + ch] & 0x10) != 0 {
                self.env_level.clamp(0, 31) as usize
            } else {
                let v = (self.regs[8 + ch] & 0x0F) as usize;
                (v * 2 + 1).min(31)
            };

            let sample = (2 * chan_out - 1) as f32 * AY_DAC[vol_idx];
            match ch {
                0 => mix_l += sample,
                1 => {
                    mix_l += sample * 0.5;
                    mix_r += sample * 0.5;
                }
                _ => mix_r += sample,
            }
        }

        ((mix_l * 0.6).clamp(-1.0, 1.0), (mix_r * 0.6).clamp(-1.0, 1.0))
    }
}

struct Session {
    song: AytSong,
    chip: AyChip,
    frame_cursor: usize,
    sample_in_frame: f64,
    samples_per_frame: f64,
    apply_regs: bool,
    chip_ticks_per_sample: f64,
    chip_ticks_cumul: f64,
    finished: bool,
    recent_mono: Vec<f32>,
    rms: f32,
    spectrum: Vec<f32>,
    tracker_monitor_lines: Vec<String>,
    track_text_lines: Vec<String>,
}

impl Session {
    fn new(song: AytSong) -> Self {
        let frame_rate = song.frame_rate.max(1) as f64;
        let samples_per_frame = SAMPLE_RATE as f64 / frame_rate;
        let chip_ticks_per_sample = (song.master_clock_hz / 8.0) / SAMPLE_RATE as f64;
        let track_text_lines = build_track_text_lines(&song);

        Self {
            song,
            chip: AyChip::new(),
            frame_cursor: 0,
            sample_in_frame: 0.0,
            samples_per_frame,
            apply_regs: true,
            chip_ticks_per_sample,
            chip_ticks_cumul: 0.0,
            finished: false,
            recent_mono: Vec::with_capacity(2048),
            rms: 0.0,
            spectrum: vec![0.0; SPECTRUM_BANDS],
            tracker_monitor_lines: vec!["Preparing stream...".to_string()],
            track_text_lines,
        }
    }

    fn next_stereo_sample(&mut self) -> Option<(f32, f32)> {
        if self.frame_cursor >= self.song.frame_count {
            self.finished = true;
            return None;
        }

        if self.apply_regs {
            for &reg in &self.song.active_registers {
                if let Some(value) = self.song.reg_frames[reg as usize].get(self.frame_cursor) {
                    self.chip.write_reg(reg as usize, *value);
                }
            }
            self.apply_regs = false;
        }

        self.chip_ticks_cumul += self.chip_ticks_per_sample;
        while self.chip_ticks_cumul >= 1.0 {
            self.chip.tick();
            self.chip_ticks_cumul -= 1.0;
        }

        let sample = self.chip.mix_sample();
        self.sample_in_frame += 1.0;
        if self.sample_in_frame >= self.samples_per_frame {
            self.sample_in_frame -= self.samples_per_frame;
            self.frame_cursor += 1;
            self.apply_regs = true;
            if self.frame_cursor >= self.song.frame_count {
                self.finished = true;
            }
        }

        Some(sample)
    }

    fn update_metrics(&mut self, mono: &[f32]) {
        if mono.is_empty() {
            return;
        }

        self.recent_mono.extend_from_slice(mono);
        if self.recent_mono.len() > 2048 {
            let drain = self.recent_mono.len() - 2048;
            self.recent_mono.drain(0..drain);
        }

        self.rms = (mono.iter().map(|v| v * v).sum::<f32>() / mono.len() as f32).sqrt();
        self.spectrum = pseudo_spectrum(&self.recent_mono, SPECTRUM_BANDS);
        self.tracker_monitor_lines = build_tracker_monitor_lines(&self.song, self.frame_cursor, 14);
    }

    fn snapshot(&self) -> AudioPlaybackSnapshot {
        let frame = self.frame_cursor.min(self.song.frame_count.saturating_sub(1));
        let position_secs = if self.song.frame_rate == 0 {
            0.0
        } else {
            frame as f64 / self.song.frame_rate as f64
        };

        AudioPlaybackSnapshot {
            rms: self.rms,
            spectrum: self.spectrum.iter().copied().collect::<Vec<_>>().into(),
            table_index: self.song.table_index_for_frame(frame) as u32,
            pattern: self.song.table_index_for_frame(frame) as u32,
            row: self.song.row_for_frame(frame) as u32,
            position_secs,
            duration_secs: self.song.duration_secs(),
            playing: !self.finished,
            tracker_monitor_lines: self
                .tracker_monitor_lines
                .iter()
                .cloned()
                .map(RString::from)
                .collect::<Vec<_>>()
                .into(),
            track_text_lines: self
                .track_text_lines
                .iter()
                .cloned()
                .map(RString::from)
                .collect::<Vec<_>>()
                .into(),
        }
    }

    fn info(&self, path: &str) -> AudioTrackInfo {
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path)
            .to_string();

        AudioTrackInfo {
            name: name.into(),
            format: "AYT".into(),
            channels: CHANNELS,
            sample_rate: SAMPLE_RATE,
            duration_secs: self.song.duration_secs(),
            songs: 1,
            tracker_text_lines: self
                .track_text_lines
                .iter()
                .cloned()
                .map(RString::from)
                .collect::<Vec<_>>()
                .into(),
        }
    }
}

fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[export_root_module]
pub fn get_library() -> AudioPluginModRef {
    AudioPluginMod {
        api_version,
        metadata,
        probe,
        open,
        read_samples,
        snapshot,
        is_finished,
        close,
    }
    .leak_into_prefix()
}

extern "C" fn api_version() -> u32 {
    KKC_AUDIO_PLUGIN_API_VERSION
}

extern "C" fn metadata() -> AudioPluginMetadata {
    AudioPluginMetadata {
        id: "ayt".into(),
        name: "AYT Decoder".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "Decodes AYT and streams PCM frames to kkc-rust audio output".into(),
        mime_types: vec!["audio/x-ayt".into()].into(),
        extensions: vec!["ayt".into()].into(),
    }
}

extern "C" fn probe(path: RStr<'_>) -> AudioPluginResult<bool> {
    wrap(|| {
        let bytes = std::fs::read(path.as_str()).map_err(|err| format!("Cannot read file: {err}"))?;
        Ok(parse_ayt(&bytes).is_ok())
    })
}

extern "C" fn open(path: RStr<'_>) -> AudioPluginResult<AudioTrackInfo> {
    wrap(|| {
        let path_text = path.as_str().to_string();
        let bytes = std::fs::read(&path_text).map_err(|err| format!("Cannot read file: {err}"))?;
        let song = parse_ayt(&bytes)?;
        let session = Session::new(song);
        let info = session.info(&path_text);

        let mut guard = sessions()
            .lock()
            .map_err(|_| "AYT session lock poisoned".to_string())?;
        guard.insert(path_text, session);

        Ok(info)
    })
}

extern "C" fn read_samples(path: RStr<'_>, frames: u32) -> AudioPluginResult<AudioPcmChunk> {
    wrap(|| {
        let mut guard = sessions()
            .lock()
            .map_err(|_| "AYT session lock poisoned".to_string())?;
        let session = guard
            .get_mut(path.as_str())
            .ok_or_else(|| "AYT session not open".to_string())?;

        if session.finished {
            return Ok(AudioPcmChunk {
                samples: RVec::new(),
                channels: CHANNELS,
                sample_rate: SAMPLE_RATE,
                finished: true,
            });
        }

        let frame_count = frames.max(1) as usize;
        let mut out = Vec::with_capacity(frame_count * CHANNELS as usize);
        let mut mono = Vec::with_capacity(frame_count);
        for _ in 0..frame_count {
            let Some((l, r)) = session.next_stereo_sample() else {
                break;
            };
            out.push(l);
            out.push(r);
            mono.push((l + r) * 0.5);
        }

        session.update_metrics(&mono);
        Ok(AudioPcmChunk {
            samples: out.into(),
            channels: CHANNELS,
            sample_rate: SAMPLE_RATE,
            finished: session.finished,
        })
    })
}

extern "C" fn snapshot(path: RStr<'_>) -> AudioPluginResult<AudioPlaybackSnapshot> {
    wrap(|| {
        let guard = sessions()
            .lock()
            .map_err(|_| "AYT session lock poisoned".to_string())?;
        let session = guard
            .get(path.as_str())
            .ok_or_else(|| "AYT session not open".to_string())?;
        Ok(session.snapshot())
    })
}

extern "C" fn is_finished(path: RStr<'_>) -> AudioPluginResult<bool> {
    wrap(|| {
        let guard = sessions()
            .lock()
            .map_err(|_| "AYT session lock poisoned".to_string())?;
        Ok(guard
            .get(path.as_str())
            .map(|session| session.finished)
            .unwrap_or(true))
    })
}

extern "C" fn close(path: RStr<'_>) -> AudioPluginResult<()> {
    wrap(|| {
        let mut guard = sessions()
            .lock()
            .map_err(|_| "AYT session lock poisoned".to_string())?;
        guard.remove(path.as_str());
        Ok(())
    })
}

fn parse_ayt(bytes: &[u8]) -> Result<AytSong, String> {
    if bytes.len() < HEADER_LEN {
        return Err("AYT file too short".into());
    }

    let version = bytes[0];
    let active_mask = read_u16(bytes, 1)?;
    let pattern_size = bytes[3] as usize;
    if pattern_size == 0 {
        return Err("Invalid AYT pattern size 0".into());
    }

    let first_seq_offset = read_u16(bytes, 4)? as usize;
    let loop_seq_offset = read_u16(bytes, 6)? as usize;
    let init_offset = read_u16(bytes, 8)? as usize;
    let nb_pattern_ptr = read_u16(bytes, 10)? as usize;
    let platform_freq = bytes[12];

    if first_seq_offset < HEADER_LEN || first_seq_offset > bytes.len() {
        return Err("Invalid AYT first sequence pointer".into());
    }
    if init_offset >= bytes.len() {
        return Err("Invalid AYT init pointer".into());
    }

    let active_registers = decode_active_registers(active_mask);
    if active_registers.is_empty() {
        return Err("AYT has no active registers".into());
    }

    let present_count = active_registers.len();
    if nb_pattern_ptr < present_count {
        return Err("AYT pointer count too small".into());
    }

    let sequence_words = nb_pattern_ptr - present_count;
    if sequence_words == 0 || sequence_words % present_count != 0 {
        return Err("Invalid AYT sequence table size".into());
    }

    let sequence_bytes_len = sequence_words
        .checked_mul(2)
        .ok_or_else(|| "AYT sequence table overflow".to_string())?;
    if first_seq_offset + sequence_bytes_len > bytes.len() {
        return Err("AYT sequence table exceeds file size".into());
    }

    let pattern_data = &bytes[HEADER_LEN..first_seq_offset];
    let sequence_data = &bytes[first_seq_offset..first_seq_offset + sequence_bytes_len];

    let sequence_count = sequence_words / present_count;
    let frame_count = sequence_count
        .checked_mul(pattern_size)
        .ok_or_else(|| "AYT frame count overflow".to_string())?;

    let (platform_id, frequency_code, frame_rate, master_clock_hz, platform_name) =
        decode_platform_freq(platform_freq);

    let (const_map, init_values) = parse_init_values(bytes, init_offset)?;

    let mut reg_frames: [Vec<u8>; AY_REG_COUNT] = array::from_fn(|reg| {
        let init = if reg == 13 {
            const_map[reg].unwrap_or(0xFF)
        } else {
            const_map[reg].unwrap_or(0)
        };
        vec![init; frame_count]
    });

    let mut seq_cursor = 0usize;
    for block in 0..sequence_count {
        let block_start = block * pattern_size;
        for &reg in &active_registers {
            let offset = read_u16(sequence_data, seq_cursor)? as usize;
            seq_cursor += 2;
            if offset + pattern_size > pattern_data.len() {
                return Err("AYT pattern pointer overflow".into());
            }
            let src = &pattern_data[offset..offset + pattern_size];
            let dst = &mut reg_frames[reg as usize][block_start..block_start + pattern_size];
            dst.copy_from_slice(src);
        }
    }

    let loop_frame = if loop_seq_offset >= first_seq_offset {
        let bytes_from_first = loop_seq_offset - first_seq_offset;
        let one_seq_bytes = present_count * 2;
        if one_seq_bytes > 0 && bytes_from_first % one_seq_bytes == 0 {
            (bytes_from_first / one_seq_bytes)
                .checked_mul(pattern_size)
                .unwrap_or(0)
                .min(frame_count)
        } else {
            0
        }
    } else {
        0
    };

    Ok(AytSong {
        version,
        active_mask,
        active_registers,
        pattern_size,
        frame_count,
        sequence_count,
        loop_frame,
        frame_rate,
        platform_id,
        platform_name,
        frequency_code,
        master_clock_hz,
        reg_frames,
        init_values,
    })
}

fn parse_init_values(bytes: &[u8], init_offset: usize) -> Result<([Option<u8>; 14], Vec<(u8, u8)>), String> {
    let mut const_map = [None; 14];
    let mut values = Vec::new();
    let mut cursor = init_offset;
    while cursor + 1 < bytes.len() {
        let reg = bytes[cursor];
        let value = bytes[cursor + 1];
        cursor += 2;

        if reg == 0xFF && value == 0xFF {
            break;
        }
        if reg < 14 {
            const_map[reg as usize] = Some(value);
            values.push((reg, value));
        }
    }
    Ok((const_map, values))
}

fn decode_active_registers(mask: u16) -> Vec<u8> {
    (0u8..14)
        .filter(|reg| {
            let bit = 15usize.saturating_sub(*reg as usize);
            bit >= 2 && ((mask >> bit) & 1) != 0
        })
        .collect()
}

fn decode_platform_freq(value: u8) -> (u8, u8, u32, f64, &'static str) {
    let platform_id = value & 0x1F;
    let frequency_code = (value >> 5) & 0x07;
    let frame_rate = match frequency_code {
        0 => 50,
        1 => 25,
        2 => 60,
        3 => 30,
        4 => 100,
        5 => 200,
        _ => 50,
    };
    let (platform_name, master_clock_hz) = match platform_id {
        0 => ("Amstrad CPC", 1_000_000.0),
        1 => ("Oric", 1_000_000.0),
        2 => ("ZXUno", 1_750_000.0),
        3 => ("Pentagon", 1_750_000.0),
        4 => ("Timex TS2068", 1_764_000.0),
        5 => ("ZX 128", 1_773_450.0),
        6 => ("MSX", 1_789_772.0),
        7 => ("Atari ST", 2_000_000.0),
        8 => ("VG5000", 1_000_000.0),
        _ => ("Unknown", 1_000_000.0),
    };
    (
        platform_id,
        frequency_code,
        frame_rate,
        master_clock_hz,
        platform_name,
    )
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16, String> {
    let lo = *data
        .get(offset)
        .ok_or_else(|| "Truncated AYT data".to_string())?;
    let hi = *data
        .get(offset + 1)
        .ok_or_else(|| "Truncated AYT data".to_string())?;
    Ok(u16::from_le_bytes([lo, hi]))
}

fn build_track_text_lines(song: &AytSong) -> Vec<String> {
    let mut out = vec![
        format!("AYT version {}.{}", song.version >> 4, song.version & 0x0F),
        format!("Platform: {} (id {})", song.platform_name, song.platform_id),
        format!("Frame rate: {} Hz (code {})", song.frame_rate, song.frequency_code),
        format!("Master clock: {:.0} Hz", song.master_clock_hz),
        format!("Pattern size: {} frames", song.pattern_size),
        format!("Sequence count: {}", song.sequence_count),
        format!("Frame count: {}", song.frame_count),
        format!("Loop frame: {}", song.loop_frame),
        format!("Active mask: 0x{:04X}", song.active_mask),
    ];

    if !song.init_values.is_empty() {
        out.push(String::new());
        out.push("Init values".into());
        out.extend(
            song.init_values
                .iter()
                .map(|(reg, value)| format!("  R{reg:02} = 0x{value:02X}")),
        );
    }

    out
}

fn build_tracker_monitor_lines(song: &AytSong, frame_cursor: usize, rows: usize) -> Vec<String> {
    if song.pattern_size == 0 || song.frame_count == 0 {
        return vec!["Pattern unavailable".into()];
    }

    let frame = frame_cursor.min(song.frame_count.saturating_sub(1));
    let seq = song.table_index_for_frame(frame);
    let row = song.row_for_frame(frame);
    let start_row = row.saturating_sub(rows / 2);
    let end_row = (start_row + rows).min(song.pattern_size);

    let mut out = Vec::with_capacity(rows);
    for current_row in start_row..end_row {
        let idx = seq
            .saturating_mul(song.pattern_size)
            .saturating_add(current_row)
            .min(song.frame_count.saturating_sub(1));
        let marker = if current_row == row { '>' } else { ' ' };
        let cols = song
            .active_registers
            .iter()
            .map(|reg| format!("R{reg:02}:{:02X}", song.reg_frames[*reg as usize][idx]))
            .collect::<Vec<_>>()
            .join(" ");
        out.push(format!("{marker}{current_row:02X} {cols}"));
    }

    while out.len() < rows {
        out.push(String::new());
    }
    out
}

fn pseudo_spectrum(samples: &[f32], bands: usize) -> Vec<f32> {
    if samples.is_empty() || bands == 0 {
        return Vec::new();
    }

    let chunk = (samples.len() / bands).max(1);
    (0..bands)
        .map(|idx| {
            let start = idx * chunk;
            let end = (start + chunk).min(samples.len());
            if start >= end {
                0.0
            } else {
                let avg = samples[start..end]
                    .iter()
                    .map(|v| v.abs())
                    .sum::<f32>()
                    / (end - start) as f32;
                (avg * 2.5).min(1.0)
            }
        })
        .collect()
}

fn wrap<T>(f: impl FnOnce() -> Result<T, String>) -> RResult<T, RString> {
    match f() {
        Ok(value) => RResult::ROk(value),
        Err(err) => RResult::RErr(err.into()),
    }
}
