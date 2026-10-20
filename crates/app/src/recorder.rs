//! Microphone capture on a dedicated thread (cpal streams are not Send).

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::io::Cursor;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

const TARGET_RATE: u32 = 16_000;

enum Cmd {
    Start(Sender<Result<()>>),
    /// Replies with WAV bytes and the recorded duration in ms.
    Stop(Sender<Result<(Vec<u8>, u64)>>),
}

pub struct Recorder {
    tx: Sender<Cmd>,
}

impl Recorder {
    pub fn spawn() -> Self {
        let (tx, rx) = channel::<Cmd>();
        std::thread::spawn(move || {
            let mut active: Option<(cpal::Stream, Arc<Mutex<Vec<f32>>>, u32, u16)> = None;
            for cmd in rx {
                match cmd {
                    Cmd::Start(reply) => {
                        if active.is_some() {
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                        match open_stream() {
                            Ok(s) => {
                                active = Some(s);
                                let _ = reply.send(Ok(()));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                    Cmd::Stop(reply) => {
                        let Some((stream, buf, rate, channels)) = active.take() else {
                            let _ = reply.send(Err(anyhow!("запись не шла")));
                            continue;
                        };
                        drop(stream);
                        let samples = std::mem::take(&mut *buf.lock().unwrap());
                        let _ = reply.send(encode_wav(&samples, rate, channels));
                    }
                }
            }
        });
        Self { tx }
    }

    pub fn start(&self) -> Result<()> {
        let (tx, rx) = channel();
        self.tx.send(Cmd::Start(tx))?;
        rx.recv()?
    }

    pub fn stop(&self) -> Result<(Vec<u8>, u64)> {
        let (tx, rx) = channel();
        self.tx.send(Cmd::Stop(tx))?;
        rx.recv()?
    }
}

fn open_stream() -> Result<(cpal::Stream, Arc<Mutex<Vec<f32>>>, u32, u16)> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or_else(|| anyhow!("микрофон не найден"))?;
    let config = device.default_input_config()?;
    let rate = config.sample_rate().0;
    let channels = config.channels();
    let buf = Arc::new(Mutex::new(Vec::<f32>::with_capacity(rate as usize * 10)));
    let err_fn = |e| eprintln!("audio error: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let b = buf.clone();
            device.build_input_stream(&config.into(), move |d: &[f32], _: &_| b.lock().unwrap().extend_from_slice(d), err_fn, None)?
        }
        cpal::SampleFormat::I16 => {
            let b = buf.clone();
            device.build_input_stream(
                &config.into(),
                move |d: &[i16], _: &_| b.lock().unwrap().extend(d.iter().map(|&s| s as f32 / 32768.0)),
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::U16 => {
            let b = buf.clone();
            device.build_input_stream(
                &config.into(),
                move |d: &[u16], _: &_| b.lock().unwrap().extend(d.iter().map(|&s| (s as f32 - 32768.0) / 32768.0)),
                err_fn,
                None,
            )?
        }
        f => return Err(anyhow!("неподдерживаемый формат микрофона: {f:?}")),
    };
    stream.play()?;
    Ok((stream, buf, rate, channels))
}

/// Downmix to mono, resample to 16 kHz, encode as 16-bit WAV.
fn encode_wav(samples: &[f32], rate: u32, channels: u16) -> Result<(Vec<u8>, u64)> {
    let ch = channels.max(1) as usize;
    let mono: Vec<f32> = samples.chunks(ch).map(|f| f.iter().sum::<f32>() / f.len() as f32).collect();
    let ratio = rate as f64 / TARGET_RATE as f64;
    let out_len = (mono.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        // Average the source window: cheap low-pass + decimation.
        let a = (i as f64 * ratio) as usize;
        let b = (((i + 1) as f64 * ratio) as usize).clamp(a + 1, mono.len());
        let s = mono[a..b].iter().sum::<f32>() / (b - a) as f32;
        out.push(s);
    }
    // Normalize quiet recordings so the transcriber hears them.
    let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-4);
    let gain = (0.9 / peak).min(8.0);
    let spec = hound::WavSpec { channels: 1, sample_rate: TARGET_RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec)?;
        for s in &out {
            w.write_sample(((s * gain).clamp(-1.0, 1.0) * 32767.0) as i16)?;
        }
        w.finalize()?;
    }
    let ms = out.len() as u64 * 1000 / TARGET_RATE as u64;
    Ok((cursor.into_inner(), ms))
}
