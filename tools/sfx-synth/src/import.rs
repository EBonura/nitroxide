// SPDX-License-Identifier: GPL-2.0-or-later
//! `sfx-synth import <manifest> <source dir> <out dir> [preview dir]`: cut the
//! crowd recordings into bank sounds.
//!
//! The recordings are Gregor Quendel's "Free Crowd Cheering Sounds" (CC BY 4.0) and Tom Haigh's "Car tire squeal
//! skid loop" (CC BY 3.0), both via OpenGameArt, and are not in this repository. The manifest
//! (`recordings.manifest`) names each source file with its size and an FNV-1a hash,
//! so the import refuses a different file, and gives the offsets, fades, loop
//! crossfade, rate and target level of each cut. Everything else is arithmetic,
//! so the same sources give the same bytes. The gain each cut was given is
//! written to `recordings.applied` beside the manifest: that is the record of what
//! was done to the recordings.
//!
//! Manifest lines, `;` separated, `#` comments:
//! `name; file; bytes; fnv64; start s; end s; rate; loop|shot; fade in s;
//! fade out s; loop crossfade s; target RMS dBFS`.

use super::*;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

struct Entry {
    name: String,
    file: String,
    bytes: u64,
    fnv: u64,
    start: f64,
    end: f64,
    rate: u32,
    looped: bool,
    fade_in: f64,
    fade_out: f64,
    xfade: f64,
    target_db: f64,
}

pub fn fnv64(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325u64, |h, x| {
        (h ^ *x as u64).wrapping_mul(0x100000001b3)
    })
}

fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split(';').map(str::trim).collect();
            assert_eq!(f.len(), 12, "manifest line needs 12 fields: {l}");
            Entry {
                name: f[0].into(),
                file: f[1].into(),
                bytes: f[2].parse().unwrap(),
                fnv: u64::from_str_radix(f[3], 16).unwrap(),
                start: f[4].parse().unwrap(),
                end: f[5].parse().unwrap(),
                rate: f[6].parse().unwrap(),
                looped: f[7] == "loop",
                fade_in: f[8].parse().unwrap(),
                fade_out: f[9].parse().unwrap(),
                xfade: f[10].parse().unwrap(),
                target_db: f[11].parse().unwrap(),
            }
        })
        .collect()
}

/// An MP3 as mono samples (the channels averaged), scaled to the 16-bit range.
fn decode_mp3(bytes: Vec<u8>) -> (u32, Vec<f64>) {
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("an MP3");
    let mut format = probed.format;
    let track = format.default_track().expect("a track");
    let id = track.id;
    let rate = track.codec_params.sample_rate.expect("a sample rate");
    let mut dec = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .expect("an MP3 decoder");
    let mut out = Vec::new();
    while let Ok(pkt) = format.next_packet() {
        if pkt.track_id() != id {
            continue;
        }
        let Ok(buf) = dec.decode(&pkt) else { continue };
        let spec = *buf.spec();
        let mut sb = SampleBuffer::<f32>::new(buf.capacity() as u64, spec);
        sb.copy_interleaved_ref(buf);
        let ch = spec.channels.count();
        for frame in sb.samples().chunks(ch) {
            out.push(frame.iter().map(|s| *s as f64).sum::<f64>() / ch as f64 * 32768.0);
        }
    }
    (rate, out)
}

/// Raised-cosine ramp, 0 to 1 over `x` in 0..1.
fn ramp(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    0.5 - 0.5 * (PI * x).cos()
}

/// The cut as 16-bit-range samples at the source rate: sliced, looped or
/// faded.
fn cut(src: &[f64], rate: u32, e: &Entry) -> Vec<f64> {
    let at = |s: f64| (s * rate as f64).round() as usize;
    let (a, b) = (at(e.start), at(e.end));
    if e.looped {
        // Equal-power crossfade of the tail that follows the loop onto its
        // head, so the last sample runs straight into the first.
        let l = b - a;
        let x = at(e.xfade);
        assert!(
            b + x <= src.len(),
            "{}: loop crossfade runs past the end",
            e.name
        );
        let mut out = src[a..b].to_vec();
        for i in 0..x {
            let w = i as f64 / x as f64;
            let (g_in, g_out) = ((w * PI / 2.0).sin(), (w * PI / 2.0).cos());
            // The head fades in over the tail that continues the loop.
            out[i] = src[a + i] * g_in + src[b + i] * g_out;
        }
        debug_assert_eq!(out.len(), l);
        out
    } else {
        let mut out = src[a..b].to_vec();
        let (fi, fo) = (at(e.fade_in).max(1), at(e.fade_out).max(1));
        let n = out.len();
        for (i, v) in out.iter_mut().enumerate() {
            *v *= ramp(i as f64 / fi as f64) * ramp((n - 1 - i) as f64 / fo as f64);
        }
        out
    }
}

pub fn run(
    manifest: &std::path::Path,
    sources: &std::path::Path,
    out: &std::path::Path,
    preview: Option<&std::path::Path>,
) {
    let entries = parse(&fs::read_to_string(manifest).expect("manifest"));
    fs::create_dir_all(out).unwrap();
    if let Some(p) = preview {
        fs::create_dir_all(p).unwrap();
    }
    let mut cache: std::collections::HashMap<String, (u32, Vec<f64>)> = Default::default();
    let mut applied = String::from(
        "# name; gain dB applied to the mono mix; decoded peak dBFS; decoded RMS dBFS\n",
    );
    println!("| sound | source | cut | rate | samples | ADPCM bytes | gain dB | peak dBFS | RMS dBFS | target RMS |");
    println!("| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for e in &entries {
        if !cache.contains_key(&e.file) {
            let bytes =
                fs::read(sources.join(&e.file)).unwrap_or_else(|err| panic!("{}: {err}", e.file));
            assert_eq!(
                bytes.len() as u64,
                e.bytes,
                "{}: not the recorded file (size)",
                e.file
            );
            assert_eq!(
                fnv64(&bytes),
                e.fnv,
                "{}: not the recorded file (hash)",
                e.file
            );
            let decoded = if e.file.ends_with(".wav") {
                let w = wav::read(&bytes).expect("a WAV");
                (w.rate, w.samples)
            } else {
                decode_mp3(bytes)
            };
            cache.insert(e.file.clone(), decoded);
        }
        let (rate, src) = &cache[&e.file];
        let pcm = cut(src, *rate, e);
        let rms = (pcm.iter().map(|x| x * x).sum::<f64>() / pcm.len() as f64)
            .sqrt()
            .max(1e-9);
        let mut gain = 10f64.powf(e.target_db / 20.0) * 32768.0 / rms;
        let mut opts = CookOptions::one_shot(e.rate);
        opts.normalize_peak = None;
        if e.looped {
            opts.looping = Looping::Whole;
        }
        let (c, decoded) = loop {
            let wav = Wav {
                rate: *rate,
                samples: pcm.iter().map(|x| x * gain).collect(),
                loop_start: None,
                loop_end: None,
                bits: 16,
            };
            let c = cook(&wav, &opts);
            let decoded = adpcm::decode(&c.adpcm);
            let peak = decoded.iter().map(|x| (*x as i32).abs()).max().unwrap_or(0);
            if peak < 32_000 {
                break (c, decoded);
            }
            gain *= 0.94;
        };
        let (peak, rms_out) = levels(&decoded);
        let gain_db = db(gain);
        println!(
            "| {} | {} | {:.2} to {:.2} s{} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} |",
            e.name,
            e.file.rsplit('/').next().unwrap_or(&e.file),
            e.start,
            e.end,
            if e.looped { ", looped" } else { "" },
            e.rate,
            decoded.len(),
            c.adpcm.len(),
            gain_db,
            peak,
            rms_out,
            e.target_db
        );
        if e.looped {
            let step = |a: i16, b: i16| (a as f64 - b as f64).abs();
            let seam = step(decoded[0], decoded[decoded.len() - 1]);
            let max_step = decoded
                .windows(2)
                .map(|w| step(w[1], w[0]))
                .fold(0.0, f64::max);
            println!(
                "|  | seam | {seam:.0} against a largest step of {max_step:.0} | | | | | | | |"
            );
            assert!(seam <= 1.15 * max_step, "{}: the loop seam clicks", e.name);
        }
        applied.push_str(&format!(
            "{}; {gain_db:.2}; {peak:.1}; {rms_out:.1}\n",
            e.name
        ));
        fs::write(
            out.join(format!("{}.psau", e.name)),
            psau(e.rate, decoded.len(), &c.adpcm),
        )
        .unwrap();
        if let Some(p) = preview {
            fs::write(
                p.join(format!("{}.wav", e.name)),
                wav::write_mono16(e.rate, &decoded),
            )
            .unwrap();
        }
    }
    fs::write(manifest.with_file_name("recordings.applied"), applied).unwrap();
}
