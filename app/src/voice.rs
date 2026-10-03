//! Audio for voice calls: microphone -> noise suppression -> Opus -> engine,
//! and engine -> jitter handling -> Opus decode -> speakers.
//!
//! Everything runs at 48 kHz mono internally (Opus' native rate, 20 ms
//! frames); devices at other rates or with more channels are converted.
//! Echo cancellation: whatever the speakers actually play is kept as a
//! reference, and the echo of it is subtracted from the microphone
//! (speexdsp's adaptive filter, 125 ms tail, plus its residual echo
//! suppressor) before noise suppression and encoding.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rim_core::engine::MediaPipe;

const RATE: u32 = 48_000;
/// 20 ms at 48 kHz.
const FRAME: usize = 960;
/// Start playing once this much audio is buffered, cap the delay at the second.
const PREBUFFER_MS: usize = 60;
const MAX_BUFFER_MS: usize = 250;

/// Linear resampler that keeps its position across blocks.
struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to as f64, pos: 0.0, prev: 0.0 }
    }

    /// Index 0 is the last sample of the previous block, then `input`.
    fn run(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let at = |k: usize| if k == 0 { self.prev } else { input[k - 1] };
        let last = input.len() as f64; // highest index
        while self.pos + 1.0 <= last {
            let i = self.pos.floor() as usize;
            let f = (self.pos - i as f64) as f32;
            out.push(at(i) + (at(i + 1) - at(i)) * f);
            self.pos += self.step;
        }
        self.pos -= last;
        self.prev = input[input.len() - 1];
    }
}

type Ring = Arc<Mutex<VecDeque<f32>>>;

/// 10 ms at 48 kHz: the block size of both the echo canceller and RNNoise.
const BLOCK: usize = 480;
/// Echo tail the filter covers: output + input latency + room.
const TAIL: i32 = 6000;

/// Echo canceller on 10 ms blocks at 48 kHz.
struct EchoCanceller {
    aec: aec_rs::Aec,
}

impl EchoCanceller {
    fn new(suppress: bool) -> Self {
        EchoCanceller { aec: aec_rs::Aec::new(&aec_rs::AecConfig { frame_size: BLOCK, filter_length: TAIL, sample_rate: RATE, enable_preprocess: suppress }) }
    }

    /// `mic` minus the echo of `far` (what the speakers played).
    fn process(&self, mic: &[f32], far: &[f32]) -> Vec<f32> {
        let to16 = |x: &f32| (x.clamp(-1.0, 1.0) * 32767.0) as i16;
        let m: Vec<i16> = mic.iter().map(to16).collect();
        let f: Vec<i16> = far.iter().map(to16).collect();
        let mut out = vec![0i16; BLOCK];
        self.aec.cancel_echo(&m, &f, &mut out);
        out.iter().map(|x| *x as f32 / 32767.0).collect()
    }
}

pub struct Voice {
    _input: cpal::Stream,
    _output: cpal::Stream,
    muted: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// Playback volume in percent.
    volume: Arc<std::sync::atomic::AtomicU32>,
}

impl Voice {
    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    pub fn set_volume(&self, percent: u32) {
        self.volume.store(percent, Ordering::Relaxed);
    }
}

/// Names of the microphones and speakers the system offers.
pub fn devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let name = |d: cpal::Device| d.description().ok().map(|x| x.name().to_string());
    let ins = host.input_devices().map(|v| v.filter_map(name).collect()).unwrap_or_default();
    let outs = host.output_devices().map(|v| v.filter_map(name).collect()).unwrap_or_default();
    (ins, outs)
}

fn pick(list: Option<impl Iterator<Item = cpal::Device>>, wanted: &str) -> Option<cpal::Device> {
    if wanted.is_empty() {
        return None;
    }
    list?.find(|d| d.description().map(|x| x.name() == wanted).unwrap_or(false))
}

/// Automatic gain for the microphone: brings quiet voices up to a steady
/// speaking level (quickly down, slowly up, never above 12x), then a soft
/// limiter so peaks do not clip.
struct Agc {
    gain: f32,
}

impl Agc {
    const TARGET: f32 = 0.12;
    const FLOOR: f32 = 0.004;

    fn run(&mut self, frame: &mut [f32]) {
        let rms = (frame.iter().map(|x| x * x).sum::<f32>() / frame.len() as f32).sqrt();
        if rms > Self::FLOOR {
            let want = (Self::TARGET / rms).clamp(1.0, 12.0);
            let rate = if want < self.gain { 0.5 } else { 0.05 };
            self.gain += (want - self.gain) * rate;
        }
        for x in frame.iter_mut() {
            *x = (*x * self.gain).tanh();
        }
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Mono f32 from any sample format / channel count.
fn push_mono<T: Copy>(data: &[T], channels: usize, conv: impl Fn(T) -> f32, ring: &Ring) {
    let mut r = ring.lock().unwrap();
    for frame in data.chunks(channels.max(1)) {
        let s: f32 = frame.iter().map(|x| conv(*x)).sum::<f32>() / frame.len() as f32;
        r.push_back(s);
    }
    // Never let the capture side pile up (e.g. while the encoder is stalled).
    let cap = RATE as usize;
    if r.len() > cap {
        let n = r.len() - cap;
        r.drain(..n);
    }
}

fn input_stream(dev: &cpal::Device, ring: Ring) -> Result<(cpal::Stream, u32), String> {
    let cfg = dev.default_input_config().map_err(|e| e.to_string())?;
    let rate = cfg.sample_rate();
    let ch = cfg.channels() as usize;
    let sc: cpal::StreamConfig = cfg.clone().into();
    let err = |e| eprintln!("microphone: {e}");
    let s = match cfg.sample_format() {
        cpal::SampleFormat::F32 => dev.build_input_stream(&sc, move |d: &[f32], _| push_mono(d, ch, |x| x, &ring), err, None),
        cpal::SampleFormat::I16 => dev.build_input_stream(&sc, move |d: &[i16], _| push_mono(d, ch, |x| x as f32 / 32768.0, &ring), err, None),
        cpal::SampleFormat::U16 => dev.build_input_stream(&sc, move |d: &[u16], _| push_mono(d, ch, |x| (x as f32 - 32768.0) / 32768.0, &ring), err, None),
        cpal::SampleFormat::I32 => dev.build_input_stream(&sc, move |d: &[i32], _| push_mono(d, ch, |x| x as f32 / 2_147_483_648.0, &ring), err, None),
        f => return Err(format!("unsupported microphone format {f:?}")),
    }
    .map_err(|e| e.to_string())?;
    Ok((s, rate))
}

fn pull<T: Copy>(data: &mut [T], channels: usize, conv: impl Fn(f32) -> T, ring: &Ring, playing: &AtomicBool, prebuffer: usize, reference: &Ring) {
    let mut r = ring.lock().unwrap();
    let mut played = Vec::with_capacity(data.len() / channels.max(1));
    if !playing.load(Ordering::Relaxed) && r.len() >= prebuffer {
        playing.store(true, Ordering::Relaxed);
    }
    let on = playing.load(Ordering::Relaxed);
    for frame in data.chunks_mut(channels.max(1)) {
        let s = if on { r.pop_front().unwrap_or(0.0) } else { 0.0 };
        played.push(s);
        for x in frame.iter_mut() {
            *x = conv(s);
        }
    }
    // Ran dry: wait for the buffer to fill again (smoother than stuttering).
    if on && r.is_empty() {
        playing.store(false, Ordering::Relaxed);
    }
    drop(r);
    // What really left the speakers: the echo canceller's reference.
    let mut f = reference.lock().unwrap();
    f.extend(played);
    let cap = prebuffer * 20;
    if f.len() > cap {
        let n = f.len() - cap;
        f.drain(..n);
    }
}

fn output_stream(dev: &cpal::Device, ring: Ring, reference: Ring) -> Result<(cpal::Stream, u32), String> {
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let rate = cfg.sample_rate();
    let ch = cfg.channels() as usize;
    let sc: cpal::StreamConfig = cfg.clone().into();
    let playing = Arc::new(AtomicBool::new(false));
    let pre = rate as usize * PREBUFFER_MS / 1000;
    let err = |e| eprintln!("speakers: {e}");
    let s = match cfg.sample_format() {
        cpal::SampleFormat::F32 => dev.build_output_stream(&sc, move |d: &mut [f32], _| pull(d, ch, |x| x, &ring, &playing, pre, &reference), err, None),
        cpal::SampleFormat::I16 => dev.build_output_stream(&sc, move |d: &mut [i16], _| pull(d, ch, |x| (x.clamp(-1.0, 1.0) * 32767.0) as i16, &ring, &playing, pre, &reference), err, None),
        cpal::SampleFormat::U16 => dev.build_output_stream(&sc, move |d: &mut [u16], _| pull(d, ch, |x| ((x.clamp(-1.0, 1.0) + 1.0) * 32767.5) as u16, &ring, &playing, pre, &reference), err, None),
        cpal::SampleFormat::I32 => dev.build_output_stream(&sc, move |d: &mut [i32], _| pull(d, ch, |x| (x.clamp(-1.0, 1.0) as f64 * 2_147_483_647.0) as i32, &ring, &playing, pre, &reference), err, None),
        f => return Err(format!("unsupported speaker format {f:?}")),
    }
    .map_err(|e| e.to_string())?;
    Ok((s, rate))
}

/// Start audio for an active call.
/// Start audio for an active call with the chosen devices ("" = system default)
/// and playback volume in percent.
pub fn start(pipe: Arc<MediaPipe>, mic_name: &str, spk_name: &str, volume_percent: u32) -> Result<Voice, String> {
    let from_net = pipe.recv.lock().unwrap().take().ok_or("audio already taken")?;
    let host = cpal::default_host();
    let mic = pick(host.input_devices().ok(), mic_name).or_else(|| host.default_input_device()).ok_or("no microphone found")?;
    let spk = pick(host.output_devices().ok(), spk_name).or_else(|| host.default_output_device()).ok_or("no speakers found")?;
    let volume = Arc::new(std::sync::atomic::AtomicU32::new(volume_percent));
    let cap: Ring = Default::default();
    let play: Ring = Default::default();
    let reference: Ring = Default::default();
    let (input, in_rate) = input_stream(&mic, cap.clone())?;
    let (output, out_rate) = output_stream(&spk, play.clone(), reference.clone())?;
    input.play().map_err(|e| e.to_string())?;
    output.play().map_err(|e| e.to_string())?;
    let muted = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    // Microphone -> denoise -> Opus -> engine.
    {
        let (muted, stop, send) = (muted.clone(), stop.clone(), pipe.send.clone());
        std::thread::spawn(move || {
            let Ok(mut enc) = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip) else { return };
            let _ = enc.set_bitrate(opus::Bitrate::Bits(28_000));
            let _ = enc.set_inband_fec(true);
            let _ = enc.set_packet_loss_perc(5);
            let mut rs = Resampler::new(in_rate, RATE);
            let mut ref_rs = Resampler::new(out_rate, RATE);
            let echo = EchoCanceller::new(true);
            let mut agc = Agc { gain: 3.0 };
            let mut far: VecDeque<f32> = VecDeque::new();
            let mut denoise = nnnoiseless::DenoiseState::new();
            let mut at48: Vec<f32> = vec![];
            let mut clean: Vec<f32> = vec![];
            let mut out = [0u8; 1500];
            let mut dn_out = [0f32; nnnoiseless::DenoiseState::FRAME_SIZE];
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
                let raw: Vec<f32> = cap.lock().unwrap().drain(..).collect();
                rs.run(&raw, &mut at48);
                let played: Vec<f32> = reference.lock().unwrap().drain(..).collect();
                let mut p48 = vec![];
                ref_rs.run(&played, &mut p48);
                far.extend(p48);
                // Keep the reference from running far ahead of the microphone.
                if far.len() > RATE as usize / 5 {
                    let n = far.len() - RATE as usize / 10;
                    far.drain(..n);
                }
                // Echo cancellation, then RNNoise (both on 10 ms blocks, RNNoise in 16-bit range).
                while at48.len() >= BLOCK {
                    let mic: Vec<f32> = at48.drain(..BLOCK).collect();
                    let far_block: Vec<f32> = (0..BLOCK).map(|_| far.pop_front().unwrap_or(0.0)).collect();
                    let chunk: Vec<f32> = echo.process(&mic, &far_block).iter().map(|x| x * 32767.0).collect();
                    denoise.process_frame(&mut dn_out, &chunk);
                    clean.extend(dn_out.iter().map(|x| x / 32767.0));
                }
                while clean.len() >= FRAME {
                    let mut frame: Vec<f32> = clean.drain(..FRAME).collect();
                    if muted.load(Ordering::Relaxed) {
                        continue;
                    }
                    agc.run(&mut frame);
                    if let Ok(n) = enc.encode_float(&frame, &mut out) {
                        if send.send(out[..n].to_vec()).is_err() {
                            return; // call over
                        }
                    }
                }
            }
        });
    }

    // Engine -> Opus -> speakers, in sequence order, concealing small gaps.
    {
        let stop = stop.clone();
        let vol = volume.clone();
        std::thread::spawn(move || {
            let Ok(mut dec) = opus::Decoder::new(RATE, opus::Channels::Mono) else { return };
            let mut rs = Resampler::new(RATE, out_rate);
            let mut next: Option<u32> = None;
            let mut pcm = vec![0f32; FRAME * 6];
            let mut at_out: Vec<f32> = vec![];
            let max = out_rate as usize * MAX_BUFFER_MS / 1000;
            while !stop.load(Ordering::Relaxed) {
                let (seq, frame) = match from_net.recv_timeout(Duration::from_millis(200)) {
                    Ok(f) => f,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => return, // call over
                };
                let expected = next.unwrap_or(seq);
                if seq < expected {
                    continue; // late: already concealed
                }
                // A few frames lost: let Opus fill them in.
                let missing = (seq - expected).min(3);
                at_out.clear();
                for _ in 0..missing {
                    if let Ok(n) = dec.decode_float(&[], &mut pcm, false) {
                        rs.run(&pcm[..n], &mut at_out);
                    }
                }
                if let Ok(n) = dec.decode_float(&frame, &mut pcm, false) {
                    rs.run(&pcm[..n], &mut at_out);
                }
                next = Some(seq.wrapping_add(1));
                let v = vol.load(Ordering::Relaxed) as f32 / 100.0;
                let mut r = play.lock().unwrap();
                r.extend(at_out.iter().map(|x| (x * v).clamp(-1.0, 1.0)));
                if r.len() > max {
                    let n = r.len() - max / 2;
                    r.drain(..n);
                }
            }
        });
    }
    Ok(Voice { _input: input, _output: output, muted, stop, volume })
}

#[cfg(test)]
mod tests {
    use super::{EchoCanceller, Resampler, BLOCK};

    fn energy(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32
    }

    /// Far-end noise plays; the microphone hears it 30 ms later, attenuated.
    /// After the filter adapts, the echo must be at least 15 dB quieter.
    #[test]
    fn echo_is_cancelled() {
        let aec = EchoCanceller::new(false);
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let delay = 48 * 30;
        let far: Vec<f32> = (0..48_000 * 6).map(|_| noise() * 0.4).collect();
        let mic: Vec<f32> = (0..far.len()).map(|i| if i >= delay { far[i - delay] * 0.5 } else { 0.0 }).collect();
        let mut out = vec![];
        for (m, f) in mic.chunks(BLOCK).zip(far.chunks(BLOCK)) {
            out.extend(aec.process(m, f));
        }
        let last = out.len() - 48_000;
        let erle = 10.0 * (energy(&mic[last..]) / energy(&out[last..]).max(1e-12)).log10();
        assert!(erle > 15.0, "echo only reduced by {erle:.1} dB");
    }

    #[test]
    fn resampler_keeps_length_and_shape() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = vec![];
        // 1 s of a slow ramp in 10 blocks
        for b in 0..10 {
            let block: Vec<f32> = (0..4410).map(|i| (b * 4410 + i) as f32 / 44_100.0).collect();
            r.run(&block, &mut out);
        }
        assert!((out.len() as i32 - 48_000).abs() <= 2, "{}", out.len());
        // monotonic ramp stays monotonic, and ends near 1.0
        assert!(out.windows(2).all(|w| w[1] >= w[0]));
        assert!((out.last().unwrap() - 1.0).abs() < 0.01);
    }
}
