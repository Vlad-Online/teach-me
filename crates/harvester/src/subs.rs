//! Subtitles → utterances (sentence-like chunks with timing).
//!
//! Two inputs: YouTube ASR json3 with per-word timing, and cue-based subtitles
//! (manual json3, SRT, VTT, whisper segments).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Utterance {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Contained [música], ♪ and similar: not speech.
    pub noisy: bool,
}

#[derive(Debug, Clone)]
struct Word {
    text: String,
    start_ms: i64,
    event_end_ms: i64,
    speaker_change: bool,
}

const SENTENCE_END: &[char] = &['.', '?', '!', '…'];
const PAUSE_MS: i64 = 600;

fn est_word_ms(text: &str) -> i64 {
    (150 + 75 * text.chars().count() as i64).clamp(250, 1000)
}

fn ends_sentence(word: &str) -> bool {
    let w = word.trim_end_matches(['"', '»', ')']);
    if !w.ends_with(SENTENCE_END) {
        return false;
    }
    // "Sr." "Dr." "Sra." are not sentence ends.
    let core = w.trim_end_matches('.').to_lowercase();
    !matches!(core.as_str(), "sr" | "sra" | "dr" | "dra" | "prof" | "eng")
}

/// Strip annotations; returns cleaned text and whether noise was found.
fn clean(text: &str) -> (String, bool) {
    let mut out = String::new();
    let mut noisy = false;
    let mut depth = 0;
    for c in text.chars() {
        match c {
            '[' | '(' => {
                depth += 1;
                noisy = true;
            }
            ']' | ')' if depth > 0 => depth -= 1,
            '♪' | '♫' => noisy = true,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.replace(">>", " ");
    let out = out.trim_start_matches(['-', '–', '—', ' ']).to_string();
    (out.split_whitespace().collect::<Vec<_>>().join(" "), noisy)
}

pub fn load(path: &Path) -> Result<Vec<Utterance>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let name = path.to_string_lossy().to_lowercase();
    if name.ends_with(".json3") {
        let v: Value = serde_json::from_str(&text)?;
        let (words, cues) = parse_json3(&v);
        if !words.is_empty() {
            return Ok(words_to_utterances(&words));
        }
        return Ok(cues_to_utterances(&cues));
    }
    if name.ends_with(".cues.json") {
        let cues: Vec<Cue> = serde_json::from_str(&text)?;
        return Ok(cues_to_utterances(&cues));
    }
    if name.ends_with(".srt") || name.ends_with(".vtt") {
        return Ok(cues_to_utterances(&parse_srt_vtt(&text)));
    }
    anyhow::bail!("unknown subtitle format: {}", path.display())
}

/// YouTube json3. ASR tracks carry per-word offsets; manual tracks are cues.
fn parse_json3(v: &Value) -> (Vec<Word>, Vec<Cue>) {
    let mut words = vec![];
    let mut cues = vec![];
    let Some(events) = v["events"].as_array() else {
        return (words, cues);
    };
    let is_asr = events.iter().any(|e| {
        e["segs"]
            .as_array()
            .is_some_and(|s| s.iter().any(|g| g.get("tOffsetMs").is_some() || g.get("acAsrConf").is_some()))
    });
    for e in events {
        let Some(segs) = e["segs"].as_array() else { continue };
        let start = e["tStartMs"].as_i64().unwrap_or(0);
        let end = start + e["dDurationMs"].as_i64().unwrap_or(0);
        if is_asr {
            for g in segs {
                let raw = g["utf8"].as_str().unwrap_or("");
                if raw.trim().is_empty() {
                    continue;
                }
                let speaker_change = raw.contains(">>") || g.get("isSpeakerChange").is_some();
                let offset = g["tOffsetMs"].as_i64().unwrap_or(0);
                for (i, tok) in raw.replace(">>", " ").split_whitespace().enumerate() {
                    words.push(Word {
                        text: tok.to_string(),
                        start_ms: start + offset,
                        event_end_ms: end,
                        speaker_change: speaker_change && i == 0,
                    });
                }
            }
        } else {
            let text: String = segs.iter().filter_map(|g| g["utf8"].as_str()).collect();
            let text = text.replace('\n', " ");
            if !text.trim().is_empty() {
                cues.push(Cue { start_ms: start, end_ms: end, text });
            }
        }
    }
    words.sort_by_key(|w| w.start_ms);
    (words, cues)
}

fn words_to_utterances(words: &[Word]) -> Vec<Utterance> {
    let mut out = vec![];
    let mut cur: Vec<&Word> = vec![];
    let flush = |cur: &mut Vec<&Word>, next: Option<&Word>, out: &mut Vec<Utterance>| {
        if cur.is_empty() {
            return;
        }
        let first = cur[0];
        let last = cur[cur.len() - 1];
        let mut end = (last.start_ms + est_word_ms(&last.text)).min(last.event_end_ms.max(last.start_ms + 250));
        if let Some(n) = next {
            end = end.min(n.start_ms - 30);
        }
        let joined = cur.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        let (text, noisy) = clean(&joined);
        if !text.is_empty() {
            out.push(Utterance { text, start_ms: first.start_ms, end_ms: end.max(first.start_ms + 200), noisy });
        }
        cur.clear();
    };
    for (i, w) in words.iter().enumerate() {
        if let Some(prev) = cur.last() {
            let pause = w.start_ms - (prev.start_ms + est_word_ms(&prev.text));
            if w.speaker_change || pause > PAUSE_MS {
                flush(&mut cur, Some(w), &mut out);
            }
        }
        cur.push(w);
        if ends_sentence(&w.text) {
            flush(&mut cur, words.get(i + 1), &mut out);
        }
    }
    flush(&mut cur, None, &mut out);
    out
}

/// Split a cue into sentences, spreading time proportionally to length.
fn split_cue(cue: &Cue) -> Vec<Cue> {
    let mut parts: Vec<String> = vec![];
    let mut buf = String::new();
    for line in cue.text.lines() {
        let line = line.trim();
        // Dialogue dash starts a new speaker.
        if (line.starts_with('-') || line.starts_with('–')) && !buf.trim().is_empty() {
            parts.push(std::mem::take(&mut buf));
        }
        for tok in line.split_whitespace() {
            buf.push_str(tok);
            buf.push(' ');
            if ends_sentence(tok) {
                parts.push(std::mem::take(&mut buf));
            }
        }
    }
    if !buf.trim().is_empty() {
        parts.push(buf);
    }
    let total: usize = parts.iter().map(|p| p.len()).sum::<usize>().max(1);
    let dur = (cue.end_ms - cue.start_ms).max(1);
    let mut t = cue.start_ms;
    parts
        .into_iter()
        .map(|p| {
            let d = dur * p.len() as i64 / total as i64;
            let c = Cue { start_ms: t, end_ms: t + d, text: p.trim().to_string() };
            t += d;
            c
        })
        .collect()
}

fn cues_to_utterances(cues: &[Cue]) -> Vec<Utterance> {
    let mut pieces: Vec<Cue> = vec![];
    for c in cues {
        // VTT tracks often repeat the previous line.
        if pieces.last().is_some_and(|p| p.text == c.text.trim()) {
            pieces.last_mut().unwrap().end_ms = c.end_ms;
            continue;
        }
        pieces.extend(split_cue(c));
    }
    let mut out = vec![];
    let mut acc: Option<Cue> = None;
    for (i, p) in pieces.iter().enumerate() {
        let a = acc.get_or_insert_with(|| Cue { start_ms: p.start_ms, end_ms: p.end_ms, text: String::new() });
        if !a.text.is_empty() {
            a.text.push(' ');
        }
        a.text.push_str(&p.text);
        a.end_ms = p.end_ms;
        let gap_next = pieces.get(i + 1).map(|n| n.start_ms - p.end_ms).unwrap_or(i64::MAX);
        let last_word = p.text.split_whitespace().last().unwrap_or("");
        if ends_sentence(last_word) || gap_next > 800 || p.text.starts_with('-') {
            let a = acc.take().unwrap();
            let (text, noisy) = clean(&a.text);
            if !text.is_empty() {
                out.push(Utterance { text, start_ms: a.start_ms, end_ms: a.end_ms, noisy });
            }
        }
    }
    if let Some(a) = acc {
        let (text, noisy) = clean(&a.text);
        if !text.is_empty() {
            out.push(Utterance { text, start_ms: a.start_ms, end_ms: a.end_ms, noisy });
        }
    }
    out
}

fn parse_ts(s: &str) -> Option<i64> {
    // 00:01:02,345 | 00:01:02.345 | 01:02.345
    let s = s.trim().replace(',', ".");
    let (hms, ms) = s.split_once('.').unwrap_or((&s, "0"));
    let parts: Vec<i64> = hms.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        _ => return None,
    };
    let ms: i64 = format!("{:0<3}", &ms[..ms.len().min(3)]).parse().ok()?;
    Some(secs * 1000 + ms)
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut in_brace = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            '{' => in_brace = true,
            '}' if in_brace => in_brace = false,
            _ if !in_tag && !in_brace => out.push(c),
            _ => {}
        }
    }
    out
}

pub fn parse_srt_vtt(text: &str) -> Vec<Cue> {
    let mut cues = vec![];
    let text = text.replace("\r\n", "\n");
    for block in text.split("\n\n") {
        let mut lines = block.lines().skip_while(|l| !l.contains("-->"));
        let Some(times) = lines.next() else { continue };
        let Some((a, b)) = times.split_once("-->") else { continue };
        let b = b.split_whitespace().next().unwrap_or("");
        let (Some(start_ms), Some(end_ms)) = (parse_ts(a), parse_ts(b)) else { continue };
        let body: Vec<String> = lines.map(strip_tags).filter(|l| !l.trim().is_empty()).collect();
        if !body.is_empty() {
            cues.push(Cue { start_ms, end_ms, text: body.join("\n") });
        }
    }
    cues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_and_sentences() {
        let srt = "1\n00:00:01,000 --> 00:00:03,000\nOlá. Tudo bem?\n\n2\n00:00:03,100 --> 00:00:05,000\n- Sim, e tu?\n- Também.\n";
        let u = cues_to_utterances(&parse_srt_vtt(srt));
        let texts: Vec<_> = u.iter().map(|u| u.text.as_str()).collect();
        assert_eq!(texts, ["Olá.", "Tudo bem?", "Sim, e tu?", "Também."]);
        assert_eq!(u[0].start_ms, 1000);
    }

    #[test]
    fn clean_noise() {
        assert_eq!(clean(">> [Música] Bom dia"), ("Bom dia".to_string(), true));
    }
}
