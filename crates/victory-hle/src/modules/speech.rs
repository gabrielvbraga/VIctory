//! Accessibility: speech synthesis for the title's narration, by the host's
//! synthesizer (WinRT `Windows.Media.SpeechSynthesis`). Each stream holds
//! the WAVE audio made for its line, in the format the silent streams have
//! always had (16 kHz mono 16-bit PCM). When the host cannot speak a line
//! (no voice installed, SSML it refuses, no answer in time) the stream is
//! valid WAVE audio of no length and that line is silent, as every line was
//! before.
// shortcut: the host's default voice speaks every line; voice enumeration
// (slot 0x40) still reports none and a custom voice (slot 0x60) is ignored.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};

/// Closed captions, high contrast, speech to text and synthesis.
pub const PROVIDER: Guid = Guid::new(
    0x3e24_1977,
    0x6237,
    0x41e9,
    [0x85, 0x59, 0x10, 0x2c, 0x2d, 0x99, 0x83, 0xf1],
);
/// Unchanged since 2020.
pub const IID: Guid = Guid::new(
    0xd722_b373,
    0x8c4d,
    0x4692,
    [0x9e, 0x51, 0xd6, 0xad, 0x9b, 0x37, 0xaa, 0x7d],
);

pub static OBJECT: Object = Object::new("accessibility", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT)];

/// A WAVE file of 16 kHz mono 16-bit PCM with no samples: RIFF size 36,
/// a 16-byte format chunk (PCM, 1 channel, 16,000 Hz, 32,000 bytes/s,
/// block 2, 16 bits), an empty data chunk.
const SILENCE: [u8; 44] =
    *b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\0\0\0\0";

/// The synthesizer holds nothing, so its handle is a fixed address and
/// closing it is free.
static SYNTHESIZER: u8 = 0;

/// Open streams by handle. Handles are opaque to the title and never
/// reused, so a stale handle (closed twice, read after closing) can only
/// read silence, never another line.
static STREAMS: LazyLock<Mutex<HashMap<usize, Arc<[u8]>>>> = LazyLock::new(Mutex::default);

/// A new stream handle: non-null, aligned, never handed out before.
fn handle() -> *const u8 {
    static NEXT: AtomicUsize = AtomicUsize::new(0x1_0000);
    NEXT.fetch_add(0x10, Ordering::Relaxed) as *const u8
}

fn streams() -> MutexGuard<'static, HashMap<usize, Arc<[u8]>>> {
    STREAMS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What `stream` holds; a handle that is not open reads as silence.
fn audio(stream: *const u8) -> Arc<[u8]> {
    streams()
        .get(&(stream as usize))
        .cloned()
        .unwrap_or_else(|| Arc::from(&SILENCE[..]))
}

type This = *const Object;

/// No voice is reported, so the callback never runs.
extern "system" fn enumerate_voices(
    _: This,
    _context: *mut c_void,
    _callback: *const c_void,
) -> HRESULT {
    S_OK
}
extern "system" fn create(_: This, out: *mut *const u8) -> HRESULT {
    put(out, &raw const SYNTHESIZER)
}
extern "system" fn close(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_default_voice(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_custom_voice(_: This, _: *const u8, _voice: *const c_char) -> HRESULT {
    S_OK
}

/// Speaks `words` (SSML with `ssml`) into a new stream.
fn open(ssml: bool, words: *const c_char, out: *mut *const u8) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let words = text(words).unwrap_or_default();
    tracing::debug!(text = words.as_str(), ssml, "speech");
    let wav = host::speak(ssml, words)
        .and_then(|wav| wave::to_title(&wav))
        .unwrap_or_else(|| SILENCE.to_vec());
    let handle = handle();
    streams().insert(handle as usize, wav.into());
    put(out, handle)
}
extern "system" fn create_stream(
    _: This,
    _: *const u8,
    words: *const c_char,
    out: *mut *const u8,
) -> HRESULT {
    open(false, words, out)
}
extern "system" fn create_ssml_stream(
    _: This,
    _: *const u8,
    ssml: *const c_char,
    out: *mut *const u8,
) -> HRESULT {
    open(true, ssml, out)
}
extern "system" fn close_stream(_: This, stream: *const u8) -> HRESULT {
    streams().remove(&(stream as usize));
    S_OK
}
extern "system" fn stream_size(_: This, stream: *const u8, out: *mut usize) -> HRESULT {
    put(out, audio(stream).len())
}
extern "system" fn stream_data(
    _: This,
    stream: *const u8,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HRESULT {
    let wav = audio(stream);
    if buffer.is_null() || size < wav.len() {
        return super::xasync::E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { buffer.copy_from_nonoverlapping(wav.as_ptr(), wav.len()) };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(wav.len()) };
    }
    S_OK
}

/// WAVE files: the host's 16-bit PCM, at whatever rate and channel count
/// its voice speaks, brought to the format of `SILENCE`.
mod wave {
    /// Rate of the title's streams, as `SILENCE` declares it.
    pub const RATE: u32 = 16_000;

    /// `wav` as 16 kHz mono 16-bit PCM, or `None` when it is not 16-bit PCM
    /// WAVE audio.
    pub fn to_title(wav: &[u8]) -> Option<Vec<u8>> {
        let (channels, rate, data) = parse(wav)?;
        let frames: Vec<i32> = data
            .chunks_exact(2 * channels)
            .map(|frame| {
                let sum: i32 = frame
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&s| i32::from(i16::from_le_bytes(s)))
                    .sum();
                sum / channels as i32
            })
            .collect();
        Some(file(&resample(&frames, rate)))
    }

    /// Channels, rate and sample bytes of a 16-bit PCM WAVE file.
    fn parse(wav: &[u8]) -> Option<(usize, u32, &[u8])> {
        if wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
            return None;
        }
        let (mut format, mut data) = (None, None);
        let mut rest = &wav[12..];
        while rest.len() >= 8 {
            let size = u32::from_le_bytes(rest[4..8].try_into().ok()?) as usize;
            let body = rest.get(8..)?;
            let body = &body[..size.min(body.len())];
            match &rest[..4] {
                b"fmt " => format = Some(body),
                b"data" => data = Some(body),
                _ => {}
            }
            // Chunks are padded to an even size.
            rest = rest.get(8 + size + (size & 1)..).unwrap_or_default();
        }
        let (format, data) = (format?, data?);
        let u16_at = |i: usize| Some(u16::from_le_bytes(format.get(i..i + 2)?.try_into().ok()?));
        let tag = u16_at(0)?;
        let channels = usize::from(u16_at(2)?);
        let rate = u32::from_le_bytes(format.get(4..8)?.try_into().ok()?);
        let bits = u16_at(14)?;
        // WAVE_FORMAT_PCM, or WAVE_FORMAT_EXTENSIBLE whose subformat GUID
        // starts with the PCM tag.
        let pcm = tag == 1 || (tag == 0xfffe && u16_at(24)? == 1);
        (pcm && bits == 16 && channels > 0 && rate > 0).then_some((channels, rate, data))
    }

    /// `frames` at `rate` brought to `RATE`: each output sample averages
    /// the input it covers when going down (so nothing above 8 kHz folds
    /// back as noise), and interpolates when going up.
    fn resample(frames: &[i32], rate: u32) -> Vec<i16> {
        if frames.is_empty() {
            return Vec::new();
        }
        let step = f64::from(rate) / f64::from(RATE);
        let len = (frames.len() as f64 / step).floor() as usize;
        (0..len)
            .map(|i| {
                let start = i as f64 * step;
                let value = if step > 1.0 {
                    let first = start as usize;
                    let last = (((i + 1) as f64 * step) as usize).clamp(first + 1, frames.len());
                    let span = &frames[first..last];
                    span.iter().map(|&s| f64::from(s)).sum::<f64>() / span.len() as f64
                } else {
                    let at = start as usize;
                    let next = frames[(at + 1).min(frames.len() - 1)];
                    let t = start - at as f64;
                    f64::from(frames[at]) * (1.0 - t) + f64::from(next) * t
                };
                value
                    .round()
                    .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
            })
            .collect()
    }

    /// A WAVE file of `samples`, laid out as `SILENCE`.
    fn file(samples: &[i16]) -> Vec<u8> {
        let data = (samples.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + samples.len() * 2);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * 2).to_le_bytes());
        wav.extend_from_slice(b"\x02\0\x10\0data");
        wav.extend_from_slice(&data.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    #[cfg(test)]
    mod tests {
        use super::super::SILENCE;
        use super::*;

        /// A WAVE file of `samples` (interleaved) at `rate`, with a chunk
        /// the host may add before the data.
        fn host_file(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
            let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            let mut wav = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0".to_vec();
            wav.extend_from_slice(&channels.to_le_bytes());
            wav.extend_from_slice(&rate.to_le_bytes());
            wav.extend_from_slice(&(rate * 2 * u32::from(channels)).to_le_bytes());
            wav.extend_from_slice(&(2 * channels).to_le_bytes());
            wav.extend_from_slice(b"\x10\0LIST\x03\0\0\0abc\0data");
            wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(&data);
            wav
        }

        #[test]
        fn no_samples_is_silence() {
            assert_eq!(to_title(&host_file(1, 22_050, &[])).unwrap(), SILENCE);
        }

        #[test]
        fn the_title_format_passes_unchanged() {
            let samples = [0, 1000, -1000, i16::MAX, i16::MIN];
            let wav = host_file(1, RATE, &samples);
            let out = to_title(&wav).unwrap();
            assert_eq!(out.len(), 44 + 10);
            // The header of `SILENCE` but for the sizes.
            assert_eq!(&out[8..36], &SILENCE[8..36]);
            assert_eq!(&out[44..], &wav[wav.len() - 10..]);
        }

        #[test]
        fn host_rates_and_channels_become_16_khz_mono() {
            // One second of a steady level, stereo at 24 kHz and mono at 22.05 kHz.
            for (channels, rate) in [(2, 24_000), (1, 22_050)] {
                let samples = vec![1200_i16; rate as usize * usize::from(channels)];
                let out = to_title(&host_file(channels, rate, &samples)).unwrap();
                assert_eq!(out.len(), 44 + 2 * RATE as usize);
                assert_eq!(
                    u32::from_le_bytes(out[40..44].try_into().unwrap()),
                    2 * RATE
                );
                assert!(
                    out[44..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .all(|&s| i16::from_le_bytes(s) == 1200)
                );
            }
        }

        #[test]
        fn other_audio_is_refused() {
            assert_eq!(to_title(b"RIFF\0\0\0\0WAVE"), None);
            let mut float = host_file(1, RATE, &[1, 2]);
            float[20] = 3; // WAVE_FORMAT_IEEE_FLOAT
            assert_eq!(to_title(&float), None);
        }
    }
}

/// The host's synthesizer, on a thread of ours: COM is initialized there
/// and never on the title's threads, whose apartments the title owns.
#[cfg(windows)]
mod host {
    use std::sync::OnceLock;
    use std::sync::mpsc::{Sender, channel};
    use std::time::Duration;

    use windows::Media::SpeechSynthesis::{SpeechSynthesisStream, SpeechSynthesizer};
    use windows::Storage::Streams::DataReader;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::{HSTRING, Result};

    /// How long a title thread waits for a line before going on silently.
    const WAIT: Duration = Duration::from_secs(10);

    type Job = (bool, String, Sender<Option<Vec<u8>>>);

    /// WAVE audio of `words` (SSML with `ssml`), or `None` when the host
    /// cannot speak them or does not within `WAIT`.
    pub fn speak(ssml: bool, words: String) -> Option<Vec<u8>> {
        static JOBS: OnceLock<Sender<Job>> = OnceLock::new();
        let jobs = JOBS.get_or_init(|| {
            let (jobs, queue) = channel::<Job>();
            let thread = std::thread::Builder::new()
                .name("victory speech".into())
                .spawn(move || {
                    // SAFETY: initializes COM on this thread, which is ours.
                    let synthesizer = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                        .ok()
                        .and_then(|()| SpeechSynthesizer::new())
                        .inspect_err(|e| tracing::warn!(error = %e, "no speech synthesizer"))
                        .ok();
                    for (ssml, words, reply) in queue {
                        let wav = synthesizer.as_ref().and_then(|s| {
                            synthesize(s, ssml, &words)
                                .inspect_err(|e| tracing::warn!(error = %e, "speech failed"))
                                .ok()
                        });
                        // The title thread may have stopped waiting.
                        let _ = reply.send(wav);
                    }
                });
            if let Err(e) = thread {
                tracing::warn!(error = %e, "no speech thread");
            }
            jobs
        });
        let (reply, answer) = channel();
        jobs.send((ssml, words, reply)).ok()?;
        answer.recv_timeout(WAIT).ok().flatten()
    }

    fn synthesize(synthesizer: &SpeechSynthesizer, ssml: bool, words: &str) -> Result<Vec<u8>> {
        let words = HSTRING::from(words);
        let stream = if ssml {
            synthesizer.SynthesizeSsmlToStreamAsync(&words)?
        } else {
            synthesizer.SynthesizeTextToStreamAsync(&words)?
        }
        .join()?;
        bytes(&stream)
    }

    /// The whole stream: a WAVE file.
    fn bytes(stream: &SpeechSynthesisStream) -> Result<Vec<u8>> {
        let size = stream.Size()? as u32;
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
        reader.LoadAsync(size)?.join()?;
        let mut wav = vec![0u8; size as usize];
        reader.ReadBytes(&mut wav)?;
        Ok(wav)
    }
}

#[cfg(not(windows))]
mod host {
    pub fn speak(_ssml: bool, _words: String) -> Option<Vec<u8>> {
        None
    }
}

static VTABLE: Methods<22> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),       // 0x18 closed captions
    missing::<4> as *const (),       // 0x20
    missing::<5> as *const (),       // 0x28 high contrast
    missing::<6> as *const (),       // 0x30 speech to text
    missing::<7> as *const (),       // 0x38
    enumerate_voices as *const (),   // 0x40
    create as *const (),             // 0x48
    close as *const (),              // 0x50
    set_default_voice as *const (),  // 0x58
    set_custom_voice as *const (),   // 0x60
    create_stream as *const (),      // 0x68 from text
    close_stream as *const (),       // 0x70 a stream
    stream_size as *const (),        // 0x78
    stream_data as *const (),        // 0x80
    missing::<17> as *const (),      // 0x88 speech to text
    missing::<18> as *const (),      // 0x90
    missing::<19> as *const (),      // 0x98
    missing::<20> as *const (),      // 0xa0
    create_ssml_stream as *const (), // 0xa8 from SSML
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    type Create = extern "system" fn(This, *mut *const u8) -> HRESULT;
    type Stream = extern "system" fn(This, *const u8, *const c_char, *mut *const u8) -> HRESULT;
    type Close = extern "system" fn(This, *const u8) -> HRESULT;
    type Size = extern "system" fn(This, *const u8, *mut usize) -> HRESULT;
    type Data = extern "system" fn(This, *const u8, usize, *mut u8, *mut usize) -> HRESULT;

    /// Reads `stream` as the title does: its size, then its bytes.
    fn read(stream: *const u8) -> Vec<u8> {
        let this: This = &OBJECT;
        let mut size = 0;
        assert_eq!(method::<Size>(&OBJECT, 0x78)(this, stream, &mut size), S_OK);
        let mut wav = vec![0u8; size];
        let mut used = 0;
        let data = method::<Data>(&OBJECT, 0x80);
        assert_eq!(data(this, stream, size, wav.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(used, size);
        wav
    }

    /// Opens a stream of `words` from `slot` (0x68 text, 0xa8 SSML).
    fn say(slot: usize, words: &std::ffi::CStr) -> *const u8 {
        let this: This = &OBJECT;
        let (mut synth, mut stream) = (std::ptr::null(), std::ptr::null());
        assert_eq!(method::<Create>(&OBJECT, 0x48)(this, &mut synth), S_OK);
        assert_eq!(
            method::<Stream>(&OBJECT, slot)(this, synth, words.as_ptr(), &mut stream),
            S_OK
        );
        stream
    }

    fn is_wave(wav: &[u8]) -> bool {
        wav.len() >= 44 && &wav[..4] == b"RIFF" && &wav[8..16] == b"WAVEfmt "
    }

    #[test]
    fn a_stream_that_is_not_open_is_an_empty_wave_file() {
        let wav = read(&raw const SYNTHESIZER);
        assert_eq!(wav, SILENCE);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
    }

    /// Spoken or, on a host without a voice, silent: WAVE audio either way,
    /// and silence again once closed.
    #[test]
    fn lines_are_wave_files_until_closed() {
        let this: This = &OBJECT;
        let close = method::<Close>(&OBJECT, 0x70);
        let text = say(0x68, c"Hello");
        let ssml = say(0xa8, c"<speak version='1.0' xml:lang='en-US'>Hello</speak>");
        assert_ne!(text, ssml);
        for stream in [text, ssml] {
            assert!(is_wave(&read(stream)));
            assert_eq!(close(this, stream), S_OK);
            assert_eq!(read(stream), SILENCE);
            // A second close is harmless.
            assert_eq!(close(this, stream), S_OK);
        }
    }

    /// Needs a voice on the host. Writes the lines to the temporary folder
    /// to listen to:
    /// `cargo test -p victory-hle -- --ignored --nocapture lines_are_spoken`.
    #[test]
    #[ignore = "needs a host voice"]
    fn lines_are_spoken() {
        let close = method::<Close>(&OBJECT, 0x70);
        let lines = [
            (0x68, c"Hello from VIctory", "victory-speech-text.wav"),
            (
                0xa8,
                c"<speak version='1.0' xml:lang='en-US'>Press <emphasis>Start</emphasis></speak>",
                "victory-speech-ssml.wav",
            ),
        ];
        for (slot, words, name) in lines {
            let stream = say(slot, words);
            let wav = read(stream);
            assert!(
                is_wave(&wav) && wav.len() > SILENCE.len(),
                "{name} is silent"
            );
            let path = std::env::temp_dir().join(name);
            std::fs::write(&path, &wav).unwrap();
            println!("{}", path.display());
            close(&OBJECT, stream);
        }
    }
}
