//! Picking popular phrases from the corpus and turning them into clips.

use anyhow::Result;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use teachme_core::db::{normalize, NewClip, NewPhrase, SourceRow};
use teachme_core::llm::Llm;
use teachme_core::openai::{OpenAi, Purpose};
use teachme_core::{Config, Db, Home};

use crate::subs::{self, Utterance};
use crate::{media, ytdlp};

#[derive(Debug, Clone)]
pub struct Candidate {
    pub source: usize,
    pub text: String,
    pub norm: String,
    pub clip_start_ms: i64,
    pub clip_end_ms: i64,
    pub prev_text: String,
    pub next_text: String,
    pub freq_score: f64,
    pub llm_score: f64,
}

fn tokens(norm: &str) -> Vec<&str> {
    norm.split(' ').filter(|w| !w.is_empty()).collect()
}

/// "eu eu", "que que": speech disfluency, bad material for a card.
fn has_stutter(text: &str) -> bool {
    let raw: Vec<&str> = text.split_whitespace().collect();
    raw.windows(2).any(|w| {
        !w[0].ends_with([',', '.', '?', '!', '…']) && w[0].to_lowercase() == w[1].trim_end_matches([',', '.', '?', '!', '…']).to_lowercase()
    })
}

fn acceptable(u: &Utterance, cfg: &Config) -> bool {
    let h = &cfg.harvest;
    let n = u.text.split_whitespace().count();
    let secs = (u.end_ms - u.start_ms) as f64 / 1000.0;
    let norm = normalize(&u.text);
    let distinct: HashSet<&str> = tokens(&norm).into_iter().collect();
    // A complete sentence: capitalized and properly terminated.
    let complete = u.text.chars().next().is_some_and(|c| c.is_uppercase())
        && u.text.trim_end_matches(['"', '»']).ends_with(['.', '?', '!', '…']);
    complete
        && distinct.len() >= h.min_words.min(3)
        && !has_stutter(&u.text)
        && !u.noisy
        && n >= h.min_words
        && n <= h.max_words
        && secs >= h.min_seconds
        && secs <= h.max_seconds
        && !u.text.chars().any(|c| c.is_ascii_digit())
        && u.text.chars().filter(|c| c.is_alphabetic()).count() >= 6
}

/// Scores every acceptable utterance by how common its words, word pairs
/// and the whole phrase are across the corpus.
pub fn build_candidates(utts: &[Vec<Utterance>], cfg: &Config, known: &HashSet<String>) -> Vec<Candidate> {
    // Corpus statistics.
    let mut word_count: HashMap<String, usize> = HashMap::new();
    let mut ngram_videos: HashMap<String, HashSet<usize>> = HashMap::new();
    let mut phrase_videos: HashMap<String, HashSet<usize>> = HashMap::new();
    for (si, list) in utts.iter().enumerate() {
        for u in list {
            let norm = normalize(&u.text);
            let toks = tokens(&norm);
            for t in &toks {
                *word_count.entry(t.to_string()).or_default() += 1;
            }
            for n in 2..=3 {
                for w in toks.windows(n) {
                    ngram_videos.entry(w.join(" ")).or_default().insert(si);
                }
            }
            phrase_videos.entry(norm).or_default().insert(si);
        }
    }
    let max_ln = word_count.values().map(|&c| (1.0 + c as f64).ln()).fold(1.0, f64::max);
    let n_videos = utts.len().max(2) as f64;

    let mut out = vec![];
    for (si, list) in utts.iter().enumerate() {
        for (i, u) in list.iter().enumerate() {
            if !acceptable(u, cfg) {
                continue;
            }
            let norm = normalize(&u.text);
            if known.contains(&norm) {
                continue;
            }
            let toks = tokens(&norm);
            let freq = toks.iter().map(|t| (1.0 + word_count[*t] as f64).ln() / max_ln).sum::<f64>() / toks.len() as f64;
            let rare = toks.iter().filter(|t| word_count[**t] <= 1).count() as f64;
            let ngram = toks
                .windows(2)
                .chain(toks.windows(3))
                .map(|w| ngram_videos.get(&w.join(" ")).map_or(0, |s| s.len()) as f64)
                .fold(0.0, f64::max);
            let ngram = (1.0 + ngram).ln() / (1.0 + n_videos).ln();
            let repeats = phrase_videos.get(&norm).map_or(1, |s| s.len()) as f64;
            let exact = (repeats - 1.0).max(0.0).ln_1p();
            let score = 0.5 * freq + 0.3 * ngram + 0.4 * exact - 0.15 * rare;

            let prev = i.checked_sub(1).and_then(|j| list.get(j));
            let next = list.get(i + 1);
            let mut clip_start = (u.start_ms - 350).max(0);
            if let Some(p) = prev {
                clip_start = clip_start.max(p.end_ms.min(u.start_ms));
            }
            let mut clip_end = u.end_ms + 500;
            if let Some(n) = next {
                clip_end = clip_end.min((n.start_ms - 30).max(u.end_ms));
            }
            out.push(Candidate {
                source: si,
                text: u.text.clone(),
                norm,
                clip_start_ms: clip_start,
                clip_end_ms: clip_end,
                prev_text: prev.map(|p| p.text.clone()).unwrap_or_default(),
                next_text: next.map(|n| n.text.clone()).unwrap_or_default(),
                freq_score: score,
                llm_score: 0.0,
            });
        }
    }
    // Same phrase in several places: keep the best-scored occurrence for now.
    out.sort_by(|a, b| b.freq_score.total_cmp(&a.freq_score));
    let mut seen = HashSet::new();
    out.retain(|c| seen.insert(c.norm.clone()));
    out
}

const RATE_PROMPT: &str = "Ты отбираешь фразы для карточек. Русскоязычный ученик уровня {level} учит европейский португальский (pt-PT) по коротким клипам из живой речи.
Оцени каждую фразу числом от 1 до 5 — насколько она полезна как самостоятельная карточка:
5 — частая живая фраза или конструкция, понятная без контекста, пригодится в разговоре;
4 — хорошая полезная фраза;
3 — нормальная, но специфичная или немного сложная;
2 — сомнительная: сильно зависит от контекста, узкая лексика;
1 — обрывок, бессмыслица без контекста, явная ошибка распознавания, в основном имена и названия, английский или бразильский португальский.
Текст получен автоматическим распознаванием речи и может содержать ошибки. prev и next — соседние реплики, только для контекста.
Верни оценку для каждого id.";

pub fn rate(api: &Llm, cands: &mut [Candidate], level: &str) -> Result<()> {
    let schema = json!({
        "type": "object",
        "properties": {
            "ratings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"id": {"type": "integer"}, "score": {"type": "integer"}},
                    "required": ["id", "score"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["ratings"],
        "additionalProperties": false
    });
    let system = RATE_PROMPT.replace("{level}", level);
    for (bi, chunk) in cands.chunks_mut(60).enumerate() {
        eprintln!("  оценка фраз: пакет {} ({} шт.)", bi + 1, chunk.len());
        let items: Vec<_> = chunk
            .iter()
            .enumerate()
            .map(|(i, c)| json!({"id": i, "text": c.text, "prev": c.prev_text, "next": c.next_text}))
            .collect();
        let v = api.json(Purpose::Enrich, &system, &json!(items).to_string(), "ratings", schema.clone())?;
        for r in v["ratings"].as_array().into_iter().flatten() {
            let (Some(id), Some(score)) = (r["id"].as_u64(), r["score"].as_f64()) else { continue };
            if let Some(c) = chunk.get_mut(id as usize) {
                c.llm_score = score;
            }
        }
    }
    Ok(())
}

/// Greedy pick by score with caps per video and per channel.
pub fn select(cands: &[Candidate], sources: &[SourceRow], want: usize) -> Vec<Candidate> {
    let mut sorted: Vec<&Candidate> = cands.iter().filter(|c| c.llm_score >= 3.0).collect();
    sorted.sort_by(|a, b| (b.llm_score + b.freq_score).total_cmp(&(a.llm_score + a.freq_score)));
    let channels: HashSet<&str> = sorted.iter().map(|c| sources[c.source].channel.as_str()).collect();
    let per_channel = (want as f64 / channels.len().max(1) as f64 * 1.5).ceil() as usize + 1;
    let per_video = 4;
    let mut by_video: HashMap<usize, usize> = HashMap::new();
    let mut by_channel: HashMap<&str, usize> = HashMap::new();
    // "O que é que …" fifteen times is boring: cap phrases sharing the first words.
    let mut by_prefix: HashMap<String, usize> = HashMap::new();
    let mut out = vec![];
    for c in sorted {
        let ch = sources[c.source].channel.as_str();
        let prefix = tokens(&c.norm).into_iter().take(3).collect::<Vec<_>>().join(" ");
        let v = by_video.entry(c.source).or_default();
        let k = by_channel.entry(ch).or_default();
        let p = by_prefix.entry(prefix).or_default();
        if *v >= per_video || *k >= per_channel || *p >= 2 {
            continue;
        }
        *v += 1;
        *k += 1;
        *p += 1;
        out.push(c.clone());
        if out.len() >= want {
            break;
        }
    }
    out
}

const ENRICH_PROMPT: &str = "Ты помогаешь русскоязычному ученику уровня {level} учить европейский португальский (pt-PT) по коротким клипам из живой речи.
Даны: распознанный текст клипа (clip_transcript), исходные автосубтитры (subtitle) и соседние реплики для контекста.
Верни:
- keep: true только если это законченная, естественная фраза живой речи, понятная без контекста и полезная ученику.
  keep: false, если: обрывок или незаконченная мысль; запинки и повторы слов («é é», «que que»); рекламный слоган или заставка канала; в основном имена или числа; смысл непонятен без контекста; английский или бразильский португальский; clip_transcript и subtitle расходятся по смыслу и нельзя уверенно понять, что именно сказано.
- text_pt: точный текст того, что звучит в клипе, с пунктуацией и заглавными буквами. Опирайся на clip_transcript (он распознан заново по звуку клипа), subtitle — менее надёжен. Исправляй только очевидные ошибки распознавания, слова не выдумывай. Если в начале или в конце попали обрывки соседних реплик, которых нет в subtitle, отбрось их.
- text_ru: естественный разговорный перевод на русский (так, как сказал бы носитель русского в этой ситуации). Сохраняй форму обращения: «tu» → «ты», «você»/глагол в 3-м лице при обращении → «вы».
- alt_ru: 2–4 других допустимых перевода с тем же смыслом (короче, другие синонимы, другой порядок).
- notes: 1–3 очень коротких пометки по-русски через «; »: ключевые слова и выражения, особенности pt-PT (например, «estar a + infinitivo», «tu» вместо «você»), если они есть. Без воды.
- cefr: примерный уровень фразы (A1, A2, B1, B2, C1).";

pub struct Enriched {
    pub keep: bool,
    pub text_pt: String,
    pub text_ru: String,
    pub alt_ru: Vec<String>,
    pub notes: String,
    pub cefr: String,
}

pub fn enrich(api: &Llm, c: &Candidate, clip_transcript: &str, level: &str) -> Result<Enriched> {
    let schema = json!({
        "type": "object",
        "properties": {
            "keep": {"type": "boolean"},
            "text_pt": {"type": "string"},
            "text_ru": {"type": "string"},
            "alt_ru": {"type": "array", "items": {"type": "string"}},
            "notes": {"type": "string"},
            "cefr": {"type": "string"}
        },
        "required": ["keep", "text_pt", "text_ru", "alt_ru", "notes", "cefr"],
        "additionalProperties": false
    });
    let user = json!({
        "clip_transcript": clip_transcript,
        "subtitle": c.text,
        "prev": c.prev_text,
        "next": c.next_text,
    });
    let v = api.json(
        Purpose::Enrich,
        &ENRICH_PROMPT.replace("{level}", level),
        &user.to_string(),
        "phrase",
        schema,
    )?;
    Ok(Enriched {
        keep: v["keep"].as_bool().unwrap_or(false),
        text_pt: v["text_pt"].as_str().unwrap_or_default().trim().to_string(),
        // Haiku occasionally leaves a stray JSON brace at the end of a string.
        text_ru: v["text_ru"].as_str().unwrap_or_default().trim().trim_end_matches(['{', '}']).trim().to_string(),
        alt_ru: v["alt_ru"]
            .as_array()
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        notes: v["notes"].as_str().unwrap_or_default().to_string(),
        cefr: v["cefr"].as_str().unwrap_or_default().to_string(),
    })
}

const SUBS_PROMPT: &str = "Это кадр из видео. Есть ли на нём вшитые субтитры — текст произносимой речи (обычно внизу кадра)?
Логотипы, названия передач, плашки с именами, бегущая строка новостей и заголовки — это НЕ субтитры.
Если субтитры есть, укажи вертикальную полосу, которую они занимают, в долях высоты кадра (0 — верх, 1 — низ), с небольшим запасом сверху и снизу.";

/// Vertical band with burned-in subtitles, if the frame has any.
fn burned_subs_band(api: &Llm, video: &Path, at_ms: i64) -> Result<Option<(f64, f64)>> {
    let jpeg = media::frame_jpeg(video, at_ms)?;
    let schema = json!({
        "type": "object",
        "properties": {
            "has_subtitles": {"type": "boolean"},
            "top": {"type": "number"},
            "bottom": {"type": "number"}
        },
        "required": ["has_subtitles", "top", "bottom"],
        "additionalProperties": false
    });
    let v = api.json_image(Purpose::Enrich, SUBS_PROMPT, "Проверь кадр.", &jpeg, "subtitles", schema)?;
    if !v["has_subtitles"].as_bool().unwrap_or(false) {
        return Ok(None);
    }
    let top = v["top"].as_f64().unwrap_or(0.7).clamp(0.0, 1.0);
    let bottom = v["bottom"].as_f64().unwrap_or(1.0).clamp(0.0, 1.0);
    if bottom <= top {
        return Ok(Some((0.65, 1.0)));
    }
    // A little extra margin: the model's estimate is rough.
    Ok(Some(((top - 0.04).max(0.0), (bottom + 0.04).min(1.0))))
}

fn safe_name(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

pub fn load_corpus(db: &Db, only_pt_pt: bool) -> Result<(Vec<SourceRow>, Vec<Vec<Utterance>>)> {
    let mut sources = vec![];
    let mut utts = vec![];
    for s in db.sources_with_subs(only_pt_pt)? {
        let Some(file) = &s.subs_file else { continue };
        match subs::load(Path::new(file)) {
            Ok(u) => {
                sources.push(s);
                utts.push(u);
            }
            Err(e) => eprintln!("  пропуск {}: {e}", s.video_id),
        }
    }
    Ok((sources, utts))
}

const VARIANT_PROMPT: &str = "Ниже — автоматические субтитры видео на португальском (выборка реплик, возможны ошибки распознавания).
Определи вариант языка:
- pt-PT — европейский португальский (Португалия);
- pt-BR — бразильский;
- mixed — заметная часть речи на бразильском варианте (гости, вставки, сюжеты) наряду с европейским;
- not-pt — в основном не португальский.
Признаки pt-PT: «estar a + инфинитив» (está a fazer), «tu» с глаголами 2-го лица, энклитика (chama-se, diz-me), лексика: autocarro, comboio, telemóvel, pequeno-almoço, fixe, miúdo, pá, rapariga, casa de banho, passadeira.
Признаки pt-BR: герундий (está fazendo), «você» в значении «ты», «a gente» вместо «nós», проклитика в начале фразы (me chama), лексика: ônibus, trem, celular, café da manhã, legal, cara, garoto, banheiro, faixa de pedestres.
Не являются признаками pt-BR: «a gente» (обычно и в Португалии), «pá», «não é?», отсутствие «tu» в монологе или новостях.
pt-BR или mixed ставь только при явных признаках (герундий в «estar + gerúndio», «você» как «ты», бразильская лексика), которые встречаются несколько раз.
Учитывай всю выборку, а не отдельные слова.";

/// Classifies every not-yet-checked video as pt-PT / pt-BR / mixed.
/// Clips already cut from non-European videos are removed.
pub fn check_variants(api: &Llm, home: &Home, db: &Db) -> Result<()> {
    let schema = json!({
        "type": "object",
        "properties": {
            "variant": {"type": "string", "enum": ["pt-PT", "pt-BR", "mixed", "not-pt"]},
            "evidence": {"type": "string"}
        },
        "required": ["variant", "evidence"],
        "additionalProperties": false
    });
    let unchecked = db.sources_unchecked()?;
    if !unchecked.is_empty() {
        eprintln!("Проверка варианта языка: {} видео", unchecked.len());
    }
    for s in unchecked {
        let Some(file) = &s.subs_file else { continue };
        let utts = match subs::load(Path::new(file)) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("  пропуск {}: {e}", s.video_id);
                continue;
            }
        };
        // ~1500 words spread over the whole video.
        let total_words: usize = utts.iter().map(|u| u.text.split_whitespace().count()).sum();
        let step = (total_words / 1500).max(1);
        let sample: Vec<&str> = utts.iter().step_by(step).map(|u| u.text.as_str()).take(400).collect();
        let user = format!("Канал: {}
Название: {}

{}", s.channel, s.title, sample.join("
"));
        let v = api.json(Purpose::Enrich, VARIANT_PROMPT, &user, "variant", schema.clone())?;
        let variant = v["variant"].as_str().unwrap_or("mixed").to_string();
        db.set_source_variant(&s.video_id, &variant)?;
        if variant == "pt-PT" {
            eprintln!("  ✓ pt-PT  {} — {}", s.channel, s.title);
        } else {
            eprintln!("  ✗ {variant}  {} — {} ({})", s.channel, s.title, v["evidence"].as_str().unwrap_or(""));
            for f in db.remove_source_clips(&s.url)? {
                let _ = std::fs::remove_file(home.clips_dir().join(&f));
                eprintln!("    удалён клип {f}");
            }
        }
    }
    Ok(())
}

/// Full mining run. Returns how many phrases were added.
pub fn run(home: &Home, cfg: &Config, db: &Db, want: usize, dry_run: bool) -> Result<usize> {
    // Text tasks go to the configured LLM; clip transcription always to OpenAI.
    let apis = if dry_run { None } else { Some((Llm::from_config(cfg)?, OpenAi::new(&cfg.openai)?)) };
    if let Some((llm, _)) = &apis {
        check_variants(llm, home, db)?;
    }
    let (sources, utts) = load_corpus(db, !dry_run)?;
    let total_utts: usize = utts.iter().map(Vec::len).sum();
    eprintln!("Корпус: {} видео, {} реплик", sources.len(), total_utts);
    let known: HashSet<String> = db.known_norms()?.into_iter().collect();
    let mut cands = build_candidates(&utts, cfg, &known);
    eprintln!("Подходящих кандидатов: {}", cands.len());
    cands.truncate((want * 8).clamp(60, 1000));

    if dry_run {
        for c in cands.iter().take(60) {
            println!("{:5.2}  [{}] {}", c.freq_score, sources[c.source].channel, c.text);
        }
        return Ok(0);
    }

    let (llm, stt) = apis.expect("apis are set unless dry run");
    rate(&llm, &mut cands, &cfg.harvest.level)?;
    // A few spares: some phrases get dropped at the enrichment step.
    let picked = select(&cands, &sources, want + want / 2 + 2);
    eprintln!("Отобрано для нарезки: {}", picked.len());

    let clips_dir = home.clips_dir();
    std::fs::create_dir_all(&clips_dir)?;
    let videos_dir = home.cache_dir().join("videos");

    let mut order: Vec<usize> = vec![];
    for c in &picked {
        if !order.contains(&c.source) {
            order.push(c.source);
        }
    }
    let mut added = 0;
    for si in order {
        if added >= want {
            break;
        }
        let src = &sources[si];
        let is_remote = src.url.starts_with("http");
        eprintln!("▶ {} — {}", src.channel, src.title);
        let video: PathBuf = if is_remote {
            match ytdlp::download_video(&src.video_id, &videos_dir) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("  не удалось скачать: {e}");
                    continue;
                }
            }
        } else {
            PathBuf::from(&src.url)
        };
        for c in picked.iter().filter(|c| c.source == si) {
            if added >= want {
                break;
            }
            match make_clip(&llm, &stt, cfg, db, &clips_dir, &video, src, c) {
                Ok(Some(text)) => {
                    added += 1;
                    eprintln!("  ✓ [{added}/{want}] {text}");
                }
                Ok(None) => {}
                Err(e) => eprintln!("  ✗ {}: {e}", c.text),
            }
        }
        db.set_source_status(&src.video_id, "mined")?;
        if is_remote && !cfg.harvest.keep_videos {
            let _ = std::fs::remove_file(&video);
        }
    }
    Ok(added)
}

fn make_clip(
    llm: &Llm,
    stt: &OpenAi,
    cfg: &Config,
    db: &Db,
    clips_dir: &Path,
    video: &Path,
    src: &SourceRow,
    c: &Candidate,
) -> Result<Option<String>> {
    let file = format!("{}_{}.mp4", safe_name(&src.video_id), c.clip_start_ms);
    let path = clips_dir.join(&file);
    let mid = (c.clip_start_ms + c.clip_end_ms) / 2;
    let blur = match burned_subs_band(llm, video, mid) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("  (проверка вшитых субтитров не удалась: {e})");
            None
        }
    };
    if let Some((top, bottom)) = blur {
        if bottom - top > 0.45 {
            eprintln!("  – отброшено (текст на весь кадр): {}", c.text);
            return Ok(None);
        }
        eprintln!("  размываю вшитые субтитры: {:.0}–{:.0}% высоты", top * 100.0, bottom * 100.0);
    }
    media::cut_clip(video, c.clip_start_ms, c.clip_end_ms, blur, &path)?;
    let result = (|| -> Result<Option<String>> {
        // No subtitle text in the prompt: it makes the model repeat ASR mistakes.
        let transcript = stt.transcribe(std::fs::read(&path)?, &file, "pt", "Português europeu (Portugal).")?;
        let e = enrich(llm, c, &transcript, &cfg.harvest.level)?;
        let distinct: HashSet<String> = tokens(&normalize(&e.text_pt)).into_iter().map(String::from).collect();
        let fragment = e.text_pt.ends_with("...") || e.text_pt.ends_with('…');
        if !e.keep || e.text_ru.is_empty() || distinct.len() < 3 || fragment || has_stutter(&e.text_pt) {
            eprintln!("  – отброшено: {}", c.text);
            return Ok(None);
        }
        let clip = NewClip {
            file: &file,
            source_url: &src.url,
            source_title: &src.title,
            start_ms: c.clip_start_ms,
            end_ms: c.clip_end_ms,
            subs_pt: &e.text_pt,
        };
        let phrase_id = match db.phrase_exists(&e.text_pt)? {
            Some(id) => id,
            None => db.insert_phrase(&NewPhrase {
                text_pt: &e.text_pt,
                text_ru: &e.text_ru,
                alt_ru: &e.alt_ru,
                notes: &e.notes,
                cefr: &e.cefr,
                usefulness: c.llm_score as i64,
            })?,
        };
        db.insert_clip(phrase_id, &clip)?;
        Ok(Some(format!("{}  →  {}", e.text_pt, e.text_ru)))
    })();
    if !matches!(result, Ok(Some(_))) {
        let _ = std::fs::remove_file(&path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(text: &str, s: i64, e: i64) -> Utterance {
        Utterance { text: text.into(), start_ms: s, end_ms: e, noisy: false }
    }

    #[test]
    fn common_phrases_rank_higher() {
        let cfg = Config::default();
        let a = vec![u("Não sei o que dizer.", 0, 1500), u("Pois é, não sei.", 2000, 3200), u("Quetzalcoatl visitou Ulaanbaatar ontem.", 4000, 6000)];
        let b = vec![u("Não sei o que dizer.", 0, 1500), u("Está bem, não faz mal.", 2000, 3500)];
        let c = build_candidates(&[a, b], &cfg, &HashSet::new());
        assert_eq!(c[0].norm, "não sei o que dizer");
        assert!(c.last().unwrap().text.starts_with("Quetzalcoatl"));
    }
}
