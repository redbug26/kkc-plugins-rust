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
use std::io::Read;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

const HEADER_LEN: usize = 12;
const AY_REGS: usize = 16;
const SAMPLE_RATE: u32 = 48_000;
const SPECTRUM_BANDS: usize = 96;
const A_STREAMINTERLEAVED: u32 = 1;

const AY_DAC: [f32; 32] = [
    0.0000000, 0.0000000, 0.0099947, 0.0099947, 0.0144503, 0.0144503, 0.0210575, 0.0210575,
    0.0307012, 0.0307012, 0.0455482, 0.0455482, 0.0644999, 0.0644999, 0.1073625, 0.1073625,
    0.1265888, 0.1265888, 0.2049897, 0.2049897, 0.2922103, 0.2922103, 0.3728389, 0.3728389,
    0.4925307, 0.4925307, 0.6353246, 0.6353246, 0.8055848, 0.8055848, 1.0000000, 1.0000000,
];

#[derive(Clone)]
struct YmSong {
    magic: [u8; 4],
    nb_frames: usize,
    attributes: u32,
    nb_drums: u16,
    clock_rate: u32,
    player_rate: u16,
    loop_frame: usize,
    song_name: String,
    song_author: String,
    song_comment: String,
    reg_frames: [Vec<u8>; AY_REGS],
}

impl YmSong {
    fn duration_secs(&self) -> f64 {
        if self.player_rate == 0 {
            0.0
        } else {
            self.nb_frames as f64 / self.player_rate as f64
        }
    }
}

#[derive(Clone)]
struct YmChip {
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

impl YmChip {
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
            11 | 12 => self.recalc_env_period(),
            13 => self.env_reload(),
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
        self.env_period = ((self.regs[11] as i32) | ((self.regs[12] as i32) << 8)).max(1);
    }

    fn env_reload(&mut self) {
        self.env_shape = (self.regs[13] & 0x0F) as i32;
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

struct YmSession {
    song: YmSong,
    chip: YmChip,
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

impl YmSession {
    fn new(song: YmSong) -> Self {
        let frame_rate = song.player_rate.max(1) as f64;
        let samples_per_frame = SAMPLE_RATE as f64 / frame_rate;
        let chip_ticks_per_sample = (song.clock_rate as f64 / 8.0) / SAMPLE_RATE as f64;
        let track_text_lines = build_track_text_lines(&song);

        Self {
            song,
            chip: YmChip::new(),
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
        if self.frame_cursor >= self.song.nb_frames {
            self.finished = true;
            return None;
        }

        if self.apply_regs {
            for reg in 0..14 {
                if let Some(value) = self.song.reg_frames[reg].get(self.frame_cursor) {
                    self.chip.write_reg(reg, *value);
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
            if self.frame_cursor >= self.song.nb_frames {
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
        let frame = self.frame_cursor.min(self.song.nb_frames.saturating_sub(1));
        let position_secs = if self.song.player_rate == 0 {
            0.0
        } else {
            frame as f64 / self.song.player_rate as f64
        };

        AudioPlaybackSnapshot {
            rms: self.rms,
            spectrum: self.spectrum.iter().copied().collect::<Vec<_>>().into(),
            table_index: frame as u32,
            pattern: frame as u32,
            row: 0,
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
            format: format!("{}", std::str::from_utf8(&self.song.magic).unwrap_or("YM"))
                .into(),
            channels: 2,
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

fn sessions() -> &'static Mutex<HashMap<String, YmSession>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, YmSession>>> = OnceLock::new();
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
        id: "ym".into(),
        name: "YM Decoder".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "Decodes YM5/YM6 and streams PCM frames to kkc-rust audio output".into(),
        mime_types: vec!["audio/x-ym".into(), "audio/x-ym6".into()].into(),
        extensions: vec!["ym".into()].into(),
    }
}

extern "C" fn probe(path: RStr<'_>) -> AudioPluginResult<bool> {
    wrap(|| {
        let path_text = path.as_str().to_string();
        let bytes = std::fs::read(&path_text).map_err(|err| format!("Cannot read file: {err}"))?;
        let parsed = parse_ym(&bytes);
        match parsed {
            Ok(_) => {
                eprintln!("[kkc-audio-ym] probe ok: {}", path_text);
                Ok(true)
            }
            Err(err) => {
                eprintln!("[kkc-audio-ym] probe failed: {} ({})", path_text, err);
                Ok(false)
            }
        }
    })
}

extern "C" fn open(path: RStr<'_>) -> AudioPluginResult<AudioTrackInfo> {
    wrap(|| {
        let path_text = path.as_str().to_string();
        let bytes = std::fs::read(&path_text).map_err(|err| format!("Cannot read file: {err}"))?;
        let song = parse_ym(&bytes).map_err(|err| {
            eprintln!("[kkc-audio-ym] open parse failed: {} ({})", path_text, err);
            err
        })?;
        let session = YmSession::new(song);
        let info = session.info(&path_text);
        eprintln!("[kkc-audio-ym] open ok: {}", path_text);

        let mut guard = sessions()
            .lock()
            .map_err(|_| "YM session lock poisoned".to_string())?;
        guard.insert(path_text, session);

        Ok(info)
    })
}

extern "C" fn read_samples(path: RStr<'_>, frames: u32) -> AudioPluginResult<AudioPcmChunk> {
    wrap(|| {
        let mut guard = sessions()
            .lock()
            .map_err(|_| "YM session lock poisoned".to_string())?;
        let session = guard
            .get_mut(path.as_str())
            .ok_or_else(|| "YM session not open".to_string())?;

        if session.finished {
            return Ok(AudioPcmChunk {
                samples: RVec::new(),
                channels: 2,
                sample_rate: SAMPLE_RATE,
                finished: true,
            });
        }

        let frame_count = frames.max(1) as usize;
        let mut out = Vec::with_capacity(frame_count * 2);
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
            channels: 2,
            sample_rate: SAMPLE_RATE,
            finished: session.finished,
        })
    })
}

extern "C" fn snapshot(path: RStr<'_>) -> AudioPluginResult<AudioPlaybackSnapshot> {
    wrap(|| {
        let guard = sessions()
            .lock()
            .map_err(|_| "YM session lock poisoned".to_string())?;
        let session = guard
            .get(path.as_str())
            .ok_or_else(|| "YM session not open".to_string())?;
        Ok(session.snapshot())
    })
}

extern "C" fn is_finished(path: RStr<'_>) -> AudioPluginResult<bool> {
    wrap(|| {
        let guard = sessions()
            .lock()
            .map_err(|_| "YM session lock poisoned".to_string())?;
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
            .map_err(|_| "YM session lock poisoned".to_string())?;
        guard.remove(path.as_str());
        Ok(())
    })
}

fn parse_ym(bytes: &[u8]) -> Result<YmSong, String> {
    let owned;
    let data = if is_lzh_compressed(bytes) {
        owned = decompress_lzh(bytes)?;
        owned.as_slice()
    } else {
        bytes
    };

    parse_ym_raw(data)
}

fn parse_ym_raw(bytes: &[u8]) -> Result<YmSong, String> {
    if bytes.len() < 4 {
        return Err("YM file too short".into());
    }

    let magic = <[u8; 4]>::try_from(&bytes[..4]).map_err(|_| "Invalid YM header".to_string())?;
    if &magic == b"YM5!" || &magic == b"YM6!" {
        return parse_ym_v5_v6(bytes, magic);
    }
    if &magic == b"YM2!" || &magic == b"YM3!" || &magic == b"YM3b" {
        return parse_ym_legacy(bytes, magic);
    }

    Err("Not a valid YM2/YM3/YM5/YM6 file".into())
}

fn parse_ym_v5_v6(bytes: &[u8], magic: [u8; 4]) -> Result<YmSong, String> {
    if bytes.len() < HEADER_LEN {
        return Err("YM file too short".into());
    }
    if &bytes[4..12] != b"LeOnArD!" {
        return Err("Invalid YM signature".into());
    }

    let mut ptr = 12usize;
    let nb_frames = read_be32(bytes, &mut ptr)? as usize;
    let attributes = read_be32(bytes, &mut ptr)?;
    let nb_drums = read_be16(bytes, &mut ptr)?;
    let clock_rate = read_be32(bytes, &mut ptr)?;
    let player_rate = read_be16(bytes, &mut ptr)?;
    let loop_frame = read_be32(bytes, &mut ptr)? as usize;
    let extra_size = read_be16(bytes, &mut ptr)? as usize;

    ptr = ptr.saturating_add(extra_size);
    if ptr > bytes.len() {
        return Err("YM extra data exceeds file size".into());
    }

    for _ in 0..nb_drums {
        let drum_size = read_be32(bytes, &mut ptr)? as usize;
        ptr = ptr.saturating_add(drum_size);
        if ptr > bytes.len() {
            return Err("YM digidrum data exceeds file size".into());
        }
    }

    let song_name = read_nt_string(bytes, &mut ptr)?;
    let song_author = read_nt_string(bytes, &mut ptr)?;
    let song_comment = read_nt_string(bytes, &mut ptr)?;

    let reg_bytes = 16usize
        .checked_mul(nb_frames)
        .ok_or_else(|| "YM register data overflow".to_string())?;
    if ptr + reg_bytes > bytes.len() {
        return Err("Not enough YM register data".into());
    }

    let mut reg_frames: [Vec<u8>; AY_REGS] = array::from_fn(|_| vec![0; nb_frames]);
    if (attributes & A_STREAMINTERLEAVED) != 0 {
        for reg in 0..AY_REGS {
            let start = ptr + reg * nb_frames;
            reg_frames[reg].copy_from_slice(&bytes[start..start + nb_frames]);
        }
    } else {
        for frame in 0..nb_frames {
            let frame_start = ptr + frame * AY_REGS;
            let frame_end = frame_start + AY_REGS;
            let src = &bytes[frame_start..frame_end];
            for reg in 0..AY_REGS {
                reg_frames[reg][frame] = src[reg];
            }
        }
    }

    Ok(YmSong {
        magic,
        nb_frames,
        attributes,
        nb_drums,
        clock_rate,
        player_rate,
        loop_frame,
        song_name,
        song_author,
        song_comment,
        reg_frames,
    })
}

fn parse_ym_legacy(bytes: &[u8], magic: [u8; 4]) -> Result<YmSong, String> {
    if bytes.len() < 4 + 14 {
        return Err("Legacy YM file too short".into());
    }

    let payload_len = bytes.len() - 4;
    let (nb_frames, loop_frame) = if &magic == b"YM3b" {
        if payload_len < 4 {
            return Err("YM3b file too short".into());
        }
        let frames = (payload_len - 4) / 14;
        let loop_bytes = &bytes[bytes.len() - 4..];
        let loop_frame = u32::from_le_bytes([loop_bytes[0], loop_bytes[1], loop_bytes[2], loop_bytes[3]]) as usize;
        (frames, loop_frame)
    } else {
        (payload_len / 14, 0)
    };

    if nb_frames == 0 {
        return Err("Legacy YM contains no frames".into());
    }

    let mut reg_frames: [Vec<u8>; AY_REGS] = array::from_fn(|_| vec![0; nb_frames]);
    let stream = &bytes[4..4 + nb_frames * 14];
    for reg in 0..14 {
        let start = reg * nb_frames;
        reg_frames[reg].copy_from_slice(&stream[start..start + nb_frames]);
    }

    Ok(YmSong {
        magic,
        nb_frames,
        attributes: A_STREAMINTERLEAVED,
        nb_drums: 0,
        clock_rate: 2_000_000,
        player_rate: 50,
        loop_frame,
        song_name: String::new(),
        song_author: String::new(),
        song_comment: String::new(),
        reg_frames,
    })
}

fn decompress_lzh(data: &[u8]) -> Result<Vec<u8>, String> {
    match decompress_lzh_strict(data) {
        Ok(out) => Ok(out),
        Err(strict_err) => {
            if let Some(repaired) = repair_lzh_level0_header_checksum(data) {
                decompress_lzh_strict(&repaired).map_err(|retry_err| {
                    format!(
                        "YM LZH decode failed (strict: {strict_err}; checksum-repair retry: {retry_err})"
                    )
                })
            } else {
                Err(strict_err)
            }
        }
    }
}

fn decompress_lzh_strict(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = delharc::LhaDecodeReader::new(data)
        .map_err(|err| format!("YM LZH header decode failed: {err}"))?;

    loop {
        if !reader.header().is_directory() {
            if !reader.is_decoder_supported() {
                return Err("YM LZH compression method is not supported".into());
            }

            let mut out = Vec::new();
            reader
                .read_to_end(&mut out)
                .map_err(|err| format!("YM LZH data decode failed: {err}"))?;
            reader
                .crc_check()
                .map_err(|err| format!("YM LZH CRC check failed: {err}"))?;
            return Ok(out);
        }

        let has_more = reader
            .next_file()
            .map_err(|err| format!("YM LZH next entry failed: {err}"))?;
        if !has_more {
            break;
        }
    }

    Err("YM LZH archive has no decodable file entry".into())
}

fn repair_lzh_level0_header_checksum(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 22 || data.get(2..5) != Some(b"-lh") || data.get(6) != Some(&b'-') {
        return None;
    }
    let header_size = data[0] as usize;
    let end = 2usize.checked_add(header_size)?;
    if end > data.len() {
        return None;
    }

    let checksum = data[2..end]
        .iter()
        .fold(0u8, |acc, byte| acc.wrapping_add(*byte));
    let mut repaired = data.to_vec();
    repaired[1] = checksum;
    Some(repaired)
}

fn is_lzh_compressed(data: &[u8]) -> bool {
    if data.len() < 22 {
        return false;
    }
    let header_size = data[0] as usize;
    header_size != 0
        && data.len() >= 22 + header_size
        && &data[2..7] == b"-lh5-"
}

fn read_be32(data: &[u8], ptr: &mut usize) -> Result<u32, String> {
    let end = ptr.saturating_add(4);
    let bytes = data.get(*ptr..end).ok_or_else(|| "Truncated YM header".to_string())?;
    *ptr = end;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_be16(data: &[u8], ptr: &mut usize) -> Result<u16, String> {
    let end = ptr.saturating_add(2);
    let bytes = data.get(*ptr..end).ok_or_else(|| "Truncated YM header".to_string())?;
    *ptr = end;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_nt_string(data: &[u8], ptr: &mut usize) -> Result<String, String> {
    let start = *ptr;
    let mut len = 0usize;
    while start + len < data.len() {
        if data[start + len] == 0 {
            let out = String::from_utf8_lossy(&data[start..start + len]).to_string();
            *ptr = start + len + 1;
            return Ok(out);
        }
        if len > 256 {
            return Err("YM string too long or missing terminator".into());
        }
        len += 1;
    }
    Err("YM string extends beyond file end".into())
}

fn build_track_text_lines(song: &YmSong) -> Vec<String> {
    let mut out = vec![
        format!("Format: {}", std::str::from_utf8(&song.magic).unwrap_or("YM")),
        format!("Frames: {}", song.nb_frames),
        format!("Player rate: {} Hz", song.player_rate),
        format!("Clock rate: {} Hz", song.clock_rate),
        format!("Loop frame: {}", song.loop_frame),
        format!("Attributes: 0x{:08X}", song.attributes),
        format!("Digidrums: {}", song.nb_drums),
    ];

    if !song.song_name.is_empty() {
        out.push(String::new());
        out.push(format!("Title: {}", song.song_name));
    }
    if !song.song_author.is_empty() {
        out.push(format!("Author: {}", song.song_author));
    }
    if !song.song_comment.is_empty() {
        out.push(String::new());
        out.push("Comment:".into());
        out.extend(song.song_comment.lines().map(|line| format!("  {line}")));
    }

    out
}

fn build_tracker_monitor_lines(song: &YmSong, frame_cursor: usize, rows: usize) -> Vec<String> {
    if song.nb_frames == 0 {
        return vec!["No YM frames".into()];
    }

    let frame = frame_cursor.min(song.nb_frames.saturating_sub(1));
    let mut lines = vec![
        format!("Frame: {} / {}", frame + 1, song.nb_frames),
        format!("Rate: {} Hz  Clock: {} Hz", song.player_rate, song.clock_rate),
        format!("Loop frame: {}", song.loop_frame),
        String::new(),
    ];

    for group in 0..7 {
        let a = group * 2;
        let b = a + 1;
        lines.push(format!(
            "R{:02}:{:02X}  R{:02}:{:02X}",
            a,
            song.reg_frames[a][frame],
            b,
            song.reg_frames[b][frame]
        ));
    }

    lines.truncate(rows.max(1));
    while lines.len() < rows {
        lines.push(String::new());
    }
    lines
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
                    .map(|value| value.abs())
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
