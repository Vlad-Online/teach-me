use anyhow::Result;
use clap::{Parser, Subcommand};

use harvester::{media, mine, sources, tools, Console};
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
    /// Изобразить телефон: найти ПК в сети и синхронизироваться в отдельную базу
    PhoneSim {
        /// Папка «телефона»: база и клипы
        #[arg(long)]
        dir: std::path::PathBuf,
        /// Ключ сопряжения (по умолчанию — ключ этого ПК)
        #[arg(long)]
        key: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = Home::locate()?;
    let cfg = home.load_config()?;
    let db = Db::open(&home.db_path())?;
    tools::refresh_path();
    match cli.cmd {
        Cmd::Discover => sources::discover(&home, &cfg, &db, &Console)?,
        Cmd::Import => sources::import(&home, &cfg, &db, &Console)?,
        Cmd::Mine { count, dry_run } => {
            let n = mine::run(&home, &cfg, &db, count, dry_run, &Console)?;
            if !dry_run {
                eprintln!("Готово: добавлено фраз {n}");
            }
        }
        Cmd::Run { count } => {
            let n = harvester::harvest(&home, &cfg, count, true, &Console)?;
            eprintln!("Готово: добавлено фраз {n}");
        }
        Cmd::CheckApi => check_api(&home, &cfg, &db)?,
        Cmd::PhoneSim { dir, key } => {
            use teachme_core::sync;
            let key = match key {
                Some(k) => k,
                None => sync::pairing_key(&db)?,
            };
            let phone = Db::open(&dir.join("phone.db"))?;
            let t = std::time::Instant::now();
            let addr = sync::discover(&phone.device_id, cfg.sync.udp_port, &[], std::time::Duration::from_secs(4))?
                .ok_or_else(|| anyhow::anyhow!("ПК в сети не найден"))?;
            println!("найден ПК {addr} за {:.1} c", t.elapsed().as_secs_f64());
            let t = std::time::Instant::now();
            let r = sync::sync_with(addr, &key, &phone, &dir.join("clips"))?;
            println!("синхронизация за {:.1} c: {}", t.elapsed().as_secs_f64(), serde_json::to_string(&r)?);
            println!("на «телефоне»: {}", serde_json::to_string(&phone.stats(Direction::PtToRu)?)?);
        }
        Cmd::Stats => {
            let s = db.stats(Direction::PtToRu)?;
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
    }
    Ok(())
}

fn check_api(home: &Home, cfg: &Config, db: &Db) -> Result<()> {
    use std::time::Instant;
    use teachme_core::judge;
    use teachme_core::llm::Llm;

    for (name, r) in [
        ("Claude", teachme_core::anthropic::Claude::list_models(&cfg.anthropic.api_key)),
        ("OpenAI", OpenAi::list_models(&cfg.openai.base_url, &cfg.openai.api_key)),
    ] {
        match r {
            Ok(m) => println!("Ключ {name}: работает, моделей {}", m.len()),
            Err(e) => println!("Ключ {name}: ошибка — {e}"),
        }
    }
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
