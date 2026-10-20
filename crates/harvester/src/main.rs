mod media;
mod mine;
mod subs;
mod ytdlp;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::path::Path;

use teachme_core::db::SourceRow;
use teachme_core::openai::OpenAi;
use teachme_core::{Config, Db, Direction, Home};

#[derive(Parser)]
#[command(about = "Сборщик базы фраз: видео → субтитры → популярные фразы → клипы")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Найти новые видео в sources.toml и скачать их субтитры
    Discover,
    /// Добавить свои видео из папки inbox/ (рядом .srt/.vtt или распознаём речь)
    Import,
    /// Отобрать популярные фразы и нарезать клипы
    Mine {
        /// Сколько новых фраз добавить
        #[arg(short, long, default_value_t = 30)]
        count: usize,
        /// Только показать лучших кандидатов, без API и нарезки
        #[arg(long)]
        dry_run: bool,
    },
    /// discover + import + mine
    Run {
        #[arg(short, long, default_value_t = 30)]
        count: usize,
    },
    /// Статистика базы
    Stats,
    /// Проверить ключи и модели реальными запросами
    CheckApi,
}

#[derive(Deserialize)]
struct SourcesFile {
    source: Vec<SourceCfg>,
}

#[derive(Deserialize)]
struct SourceCfg {
    name: String,
    kind: String,
    url: String,
    max_videos: Option<usize>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = Home::locate()?;
    let cfg = home.load_config()?;
    let db = Db::open(&home.db_path())?;
    match cli.cmd {
        Cmd::Discover => discover(&home, &cfg, &db)?,
        Cmd::Import => import(&home, &cfg, &db)?,
        Cmd::Mine { count, dry_run } => {
            let n = mine::run(&home, &cfg, &db, count, dry_run)?;
            if !dry_run {
                eprintln!("Готово: добавлено фраз {n}");
            }
        }
        Cmd::Run { count } => {
            discover(&home, &cfg, &db)?;
            import(&home, &cfg, &db)?;
            let n = mine::run(&home, &cfg, &db, count, false)?;
            eprintln!("Готово: добавлено фраз {n}");
        }
        Cmd::CheckApi => check_api(&home, &cfg, &db)?,
        Cmd::Stats => {
            let s = db.stats(Direction::PtToRu)?;
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
    }
    Ok(())
}

fn discover(home: &Home, cfg: &Config, db: &Db) -> Result<()> {
    let text = std::fs::read_to_string(home.sources_path()).context("reading sources.toml")?;
    let file: SourcesFile = toml::from_str(&text).context("parsing sources.toml")?;
    let subs_dir = home.cache_dir().join("subs");
    std::fs::create_dir_all(&subs_dir)?;
    let http = reqwest::blocking::Client::new();
    let h = &cfg.harvest;
    for src in &file.source {
        eprintln!("▶ {}", src.name);
        let videos = match ytdlp::list_videos(&src.url, src.max_videos.unwrap_or(h.videos_per_source)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  ошибка: {e}");
                continue;
            }
        };
        for v in videos {
            if db.source_status(&v.id)?.is_some() {
                continue;
            }
            let mins = v.duration / 60.0;
            if v.duration > 0.0 && (mins < h.min_video_minutes || mins > h.max_video_minutes) {
                continue;
            }
            let mut row = SourceRow {
                video_id: v.id.clone(),
                url: ytdlp::watch_url(&v.id),
                channel: src.name.clone(),
                title: v.title.clone(),
                kind: src.kind.clone(),
                duration: v.duration,
                subs_file: None,
                status: "skip".into(),
            };
            let info = match ytdlp::video_info(&v.id) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("  {}: {e}", v.id);
                    continue;
                }
            };
            if let Some(t) = info["title"].as_str() {
                row.title = t.to_string();
            }
            match ytdlp::choose_subs(&info) {
                Some((track, url)) => {
                    let path = subs_dir.join(format!("{}.json3", v.id));
                    let body = http.get(&url).send().and_then(|r| r.error_for_status()).and_then(|r| r.bytes());
                    match body {
                        Ok(b) => {
                            std::fs::write(&path, &b)?;
                            row.subs_file = Some(path.to_string_lossy().into_owned());
                            row.status = "new".into();
                            eprintln!("  + {} [{track}] {}", v.id, row.title);
                        }
                        Err(e) => eprintln!("  {}: субтитры не скачались: {e}", v.id),
                    }
                }
                None => eprintln!("  – {} не на португальском или без субтитров", v.id),
            }
            db.upsert_source(&row)?;
        }
    }
    Ok(())
}

fn check_api(home: &Home, cfg: &Config, db: &Db) -> Result<()> {
    use std::time::Instant;
    use teachme_core::judge;
    use teachme_core::llm::Llm;

    println!("Тексты: provider = {}", cfg.llm.provider);
    let llm = Llm::from_config(cfg)?;
    let card = |pt: &str, ru: &str| teachme_core::StudyCard {
        phrase_id: 0,
        clip_id: 0,
        clip_file: String::new(),
        text_pt: pt.into(),
        text_ru: ru.into(),
        alt_ru: vec![],
        notes: String::new(),
        cefr: String::new(),
        source_title: String::new(),
        is_new: true,
        difficult: false,
        reps: 0,
        lapses: 0,
    };
    let cases = [
        ("Eu vou começar e depois tu segues.", "Я начну, а потом ты продолжишь.", "я начну, а ты потом подхватишь"),
        ("Não foi só por isso.", "Это было не только из-за этого.", "это было из-за этого"),
        ("Até para a semana.", "До следующей недели.", "увидимся на следующей неделе"),
        ("Eu não estou a perceber.", "Я не понимаю.", "я не понял"),
    ];
    for (pt, ru, answer) in cases {
        let t = Instant::now();
        let v = judge::check(Some(&llm), &card(pt, ru), Direction::PtToRu, answer)?;
        println!("  [{:.1} c] {pt} | «{answer}» → {} {}", t.elapsed().as_secs_f64(), v.verdict, v.feedback);
    }

    // Vision: a frame from any existing clip.
    let clip = db
        .conn
        .query_row("SELECT file FROM clips LIMIT 1", [], |r| r.get::<_, String>(0))
        .ok();
    if let Some(file) = clip {
        let jpeg = media::frame_jpeg(&home.clips_dir().join(&file), 500)?;
        let t = Instant::now();
        let v = llm.json_image(
            teachme_core::openai::Purpose::Enrich,
            "Опиши кадр одной короткой фразой по-русски.",
            "Что на кадре?",
            &jpeg,
            "frame",
            serde_json::json!({
                "type": "object",
                "properties": {"description": {"type": "string"}},
                "required": ["description"],
                "additionalProperties": false
            }),
        )?;
        println!("  [{:.1} c] кадр {file}: {}", t.elapsed().as_secs_f64(), v["description"]);
    }

    println!("Распознавание речи: OpenAI {}", cfg.openai.transcribe_model);
    let stt = OpenAi::new(&cfg.openai)?;
    if let Ok(file) = db.conn.query_row("SELECT file FROM clips LIMIT 1", [], |r| r.get::<_, String>(0)) {
        let t = Instant::now();
        let text = stt.transcribe(std::fs::read(home.clips_dir().join(&file))?, &file, "pt", "Português europeu (Portugal).")?;
        println!("  [{:.1} c] {file}: {text}", t.elapsed().as_secs_f64());
    }
    Ok(())
}

const VIDEO_EXT: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "m4v"];

fn sidecar_subs(video: &Path) -> Option<std::path::PathBuf> {
    let dir = video.parent()?;
    let stem = video.file_stem()?.to_string_lossy().to_string();
    let mut found = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        let name = p.file_name()?.to_string_lossy().to_string();
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if (ext == "srt" || ext == "vtt") && name.starts_with(&stem) {
            // Prefer tracks marked as Portuguese: movie.pt.srt, movie.por.srt
            let lname = name.to_lowercase();
            if lname.contains(".pt") || lname.contains(".por") {
                return Some(p);
            }
            found.get_or_insert(p);
        }
    }
    found
}

fn import(home: &Home, cfg: &Config, db: &Db) -> Result<()> {
    let inbox = home.inbox_dir();
    std::fs::create_dir_all(&inbox)?;
    let subs_dir = home.cache_dir().join("subs");
    std::fs::create_dir_all(&subs_dir)?;
    let mut api: Option<OpenAi> = None;
    for entry in std::fs::read_dir(&inbox)?.flatten() {
        let path = entry.path();
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !VIDEO_EXT.contains(&ext.as_str()) {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let id = format!("file:{name}");
        if db.source_status(&id)?.is_some() {
            continue;
        }
        eprintln!("▶ inbox: {name}");
        let subs_file = match sidecar_subs(&path) {
            Some(s) => s,
            None => {
                if api.is_none() {
                    api = Some(OpenAi::new(&cfg.openai)?);
                }
                eprintln!("  субтитров нет — распознаю речь (whisper)");
                let tmp = home.cache_dir().join("audio");
                let mut cues = vec![];
                for (offset, chunk) in media::audio_chunks(&path, &tmp, 20.0 * 60.0)? {
                    let v = api.as_ref().unwrap().transcribe_segments(std::fs::read(&chunk)?, "audio.mp3", "pt")?;
                    for s in v["segments"].as_array().into_iter().flatten() {
                        cues.push(subs::Cue {
                            start_ms: ((offset + s["start"].as_f64().unwrap_or(0.0)) * 1000.0) as i64,
                            end_ms: ((offset + s["end"].as_f64().unwrap_or(0.0)) * 1000.0) as i64,
                            text: s["text"].as_str().unwrap_or_default().trim().to_string(),
                        });
                    }
                    let _ = std::fs::remove_file(&chunk);
                }
                let safe: String = name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
                let out = subs_dir.join(format!("{safe}.cues.json"));
                std::fs::write(&out, serde_json::to_string(&cues)?)?;
                out
            }
        };
        db.upsert_source(&SourceRow {
            video_id: id,
            url: path.to_string_lossy().into_owned(),
            channel: "Свои файлы".into(),
            title: name,
            kind: "file".into(),
            duration: media::duration_secs(&path).unwrap_or(0.0),
            subs_file: Some(subs_file.to_string_lossy().into_owned()),
            status: "new".into(),
        })?;
    }
    Ok(())
}
