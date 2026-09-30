//! Screen sharing in a call: capture + H.264 encode on the sharing side,
//! decode + show on the viewing side. The engine moves the frames.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, UsageType};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use openh264::OpenH264API;
use rim_core::engine::{ScreenFrame, ScreenPipe};
use rim_core::{Command, EngineHandle};
use tokio::sync::mpsc as tokio_mpsc;

/// Frames per second while sharing: sharp text matters more than motion.
const FPS: u64 = 8;
/// Wider screens are scaled down to this width.
const MAX_WIDTH: u32 = 1920;
/// A fresh keyframe at least this often, so a viewer never stays broken long.
const KEY_EVERY: Duration = Duration::from_secs(10);
/// Skip capturing while this many parts are still on their way.
const MAX_IN_FLIGHT: usize = 8;

pub struct Share {
    stop: Arc<AtomicBool>,
    send: tokio_mpsc::UnboundedSender<ScreenFrame>,
    want_key: Arc<AtomicBool>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

impl Share {
    /// Share another monitor without ending the share.
    pub fn switch(&mut self, index: usize) {
        self.stop.store(true, Ordering::Relaxed);
        self.stop = Arc::new(AtomicBool::new(false));
        self.want_key.store(true, Ordering::Relaxed);
        capture(index, self.stop.clone(), self.send.clone(), self.want_key.clone(), self.in_flight.clone());
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The monitors we can share: (name, is primary).
pub fn monitors() -> Vec<(String, bool)> {
    xcap::Monitor::all()
        .map(|v| v.iter().map(|m| (m.name().unwrap_or_default(), m.is_primary().unwrap_or(false))).collect())
        .unwrap_or_default()
}

fn encoder(width: u32) -> Option<Encoder> {
    // Bitrate grows with the picture; text needs sharpness more than motion.
    let bps = (width.max(640) * 1100).min(2_500_000);
    let cfg = EncoderConfig::new().usage_type(UsageType::ScreenContentRealTime).bitrate(BitRate::from_bps(bps)).max_frame_rate(FrameRate::from_hz(FPS as f32));
    Encoder::with_api_config(OpenH264API::from_source(), cfg).ok()
}

/// Capture and encode monitor `index` until the call or the sharing ends.
pub fn start_share(pipe: Arc<ScreenPipe>, index: usize) -> Result<Share, String> {
    let send = pipe.send.clone().ok_or("not a sending pipe")?;
    let (want_key, in_flight) = (pipe.want_key.clone(), pipe.in_flight.clone());
    drop(pipe);
    if xcap::Monitor::all().map(|v| v.len()).unwrap_or(0) == 0 {
        return Err("no screen to share".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    capture(index, stop.clone(), send.clone(), want_key.clone(), in_flight.clone());
    Ok(Share { stop, send, want_key, in_flight })
}

fn capture(index: usize, stop2: Arc<AtomicBool>, send: tokio_mpsc::UnboundedSender<ScreenFrame>, want_key: Arc<AtomicBool>, in_flight: Arc<std::sync::atomic::AtomicUsize>) {
    std::thread::spawn(move || {
        let Ok(mons) = xcap::Monitor::all() else { return };
        let Some(mon) = mons.get(index).or_else(|| mons.first()) else { return };
        let mut enc: Option<(Encoder, u32, u32)> = None;
        let mut last_key = Instant::now();
        let period = Duration::from_millis(1000 / FPS);
        while !stop2.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            if in_flight.load(Ordering::Relaxed) > MAX_IN_FLIGHT {
                std::thread::sleep(Duration::from_millis(40));
                continue;
            }
            let Ok(mut img) = mon.capture_image() else {
                std::thread::sleep(period);
                continue;
            };
            if img.width() > MAX_WIDTH {
                let h = (img.height() as u64 * MAX_WIDTH as u64 / img.width() as u64) as u32;
                img = xcap::image::imageops::resize(&img, MAX_WIDTH, h, xcap::image::imageops::FilterType::Triangle);
            }
            // H.264 wants even dimensions.
            let (w, h) = (img.width() & !1, img.height() & !1);
            if w != img.width() || h != img.height() {
                img = xcap::image::imageops::crop_imm(&img, 0, 0, w, h).to_image();
            }
            if enc.as_ref().map(|(_, ew, eh)| (*ew, *eh) != (w, h)).unwrap_or(true) {
                let Some(e) = encoder(w) else { return };
                enc = Some((e, w, h));
                want_key.store(true, Ordering::Relaxed);
            }
            let (e, _, _) = enc.as_mut().unwrap();
            if want_key.swap(false, Ordering::Relaxed) || last_key.elapsed() > KEY_EVERY {
                e.force_intra_frame();
                last_key = Instant::now();
            }
            let yuv = YUVBuffer::from_rgb_source(RgbaSliceU8::new(img.as_raw(), (w as usize, h as usize)));
            let Ok(bs) = e.encode(&yuv) else { continue };
            let key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
            let data = bs.to_vec();
            if !data.is_empty() && send.send(ScreenFrame { seq: 0, key, data }).is_err() {
                return; // sharing or call over
            }
            if let Some(rest) = period.checked_sub(t0.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    });
}

/// Decode frames and hand each finished picture to `show` (on any thread).
pub fn start_view(pipe: Arc<ScreenPipe>, engine: EngineHandle, show: impl Fn(slint::SharedPixelBuffer<slint::Rgba8Pixel>) + Send + 'static) -> Result<Arc<AtomicBool>, String> {
    let rx = pipe.recv.lock().unwrap().take().ok_or("screen already taken")?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    std::thread::spawn(move || {
        let Ok(mut dec) = Decoder::new() else { return };
        let mut need_key = true;
        let mut asked = Instant::now() - Duration::from_secs(5);
        let mut ask = |need: bool| {
            if need && asked.elapsed() > Duration::from_secs(1) {
                asked = Instant::now();
                engine.send(Command::ScreenWantKey);
            }
        };
        while !stop2.load(Ordering::Relaxed) {
            let f = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(f) => f,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    ask(need_key);
                    continue;
                }
                Err(_) => return, // sharing or call over
            };
            if need_key && !f.key {
                ask(true);
                continue;
            }
            match dec.decode(&f.data) {
                Ok(Some(yuv)) => {
                    need_key = false;
                    let (w, h) = yuv.dimensions();
                    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w as u32, h as u32);
                    yuv.write_rgba8(buf.make_mut_bytes());
                    show(buf);
                }
                Ok(None) => {}
                Err(_) => {
                    need_key = true;
                    ask(true);
                }
            }
        }
    });
    Ok(stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic "screen" survives encode -> decode at the right size.
    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (640usize, 360usize);
        let mut rgba = vec![0u8; w * h * 4];
        for (i, px) in rgba.chunks_mut(4).enumerate() {
            let (x, y) = (i % w, i / w);
            px.copy_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        let mut enc = encoder(w as u32).unwrap();
        let yuv = YUVBuffer::from_rgb_source(RgbaSliceU8::new(&rgba, (w, h)));
        let bs = enc.encode(&yuv).unwrap();
        assert!(matches!(bs.frame_type(), FrameType::IDR | FrameType::I));
        let data = bs.to_vec();
        let mut dec = Decoder::new().unwrap();
        let pic = dec.decode(&data).unwrap().expect("a picture");
        assert_eq!(pic.dimensions(), (w, h));
        let mut out = vec![0u8; w * h * 4];
        pic.write_rgba8(&mut out);
        // colours survive roughly (lossy codec)
        let (x, y) = (200, 100);
        let i = (y * w + x) * 4;
        assert!((out[i] as i32 - 200).abs() < 24 && (out[i + 1] as i32 - 100).abs() < 24, "{:?}", &out[i..i + 4]);
    }
}
