use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::srs::{self, Direction, Outcome};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS phrases (
    id          INTEGER PRIMARY KEY,
    text_pt     TEXT NOT NULL,
    norm_pt     TEXT NOT NULL UNIQUE,
    text_ru     TEXT NOT NULL,
    alt_ru      TEXT NOT NULL DEFAULT '[]',
    notes       TEXT NOT NULL DEFAULT '',
    cefr        TEXT NOT NULL DEFAULT '',
    usefulness  INTEGER NOT NULL DEFAULT 3,
    created_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS clips (
    id           INTEGER PRIMARY KEY,
    phrase_id    INTEGER NOT NULL REFERENCES phrases(id),
    file         TEXT NOT NULL,
    source_url   TEXT NOT NULL,
    source_title TEXT NOT NULL DEFAULT '',
    start_ms     INTEGER NOT NULL,
    end_ms       INTEGER NOT NULL,
    subs_pt      TEXT NOT NULL,
    shown        INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS clips_phrase ON clips(phrase_id);
CREATE TABLE IF NOT EXISTS reviews (
    phrase_id      INTEGER NOT NULL REFERENCES phrases(id),
    direction      INTEGER NOT NULL,
    due            TEXT NOT NULL,
    stability      REAL NOT NULL,
    difficulty     REAL NOT NULL,
    elapsed_days   INTEGER NOT NULL,
    scheduled_days INTEGER NOT NULL,
    reps           INTEGER NOT NULL,
    lapses         INTEGER NOT NULL,
    state          INTEGER NOT NULL,
    last_review    TEXT NOT NULL,
    first_review   TEXT NOT NULL,
    difficult      INTEGER NOT NULL DEFAULT 0,
    streak         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (phrase_id, direction)
);
CREATE TABLE IF NOT EXISTS attempts (
    id          INTEGER PRIMARY KEY,
    phrase_id   INTEGER NOT NULL,
    direction   INTEGER NOT NULL,
    ts          TEXT NOT NULL,
    attempt_no  INTEGER NOT NULL,
    answer      TEXT NOT NULL,
    verdict     TEXT NOT NULL,
    feedback    TEXT NOT NULL DEFAULT '',
    latency_ms  INTEGER NOT NULL DEFAULT 0
);
-- How each card ended, on any device. The source of truth for progress:
-- `reviews` is a cache recomputed from these events (see Db::recompute).
CREATE TABLE IF NOT EXISTS events (
    id          TEXT PRIMARY KEY,
    device      TEXT NOT NULL,
    phrase_uid  TEXT NOT NULL,
    direction   INTEGER NOT NULL,
    ts          TEXT NOT NULL,
    outcome     TEXT NOT NULL,
    answer      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS events_phrase ON events(phrase_uid, direction);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sources (
    video_id     TEXT PRIMARY KEY,
    url          TEXT NOT NULL,
    channel      TEXT NOT NULL DEFAULT '',
    title        TEXT NOT NULL DEFAULT '',
    kind         TEXT NOT NULL DEFAULT '',
    duration     REAL NOT NULL DEFAULT 0,
    subs_file    TEXT,
    status       TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
"#;

pub struct Db {
    pub conn: Connection,
    /// This installation, e.g. "desktop-3f9a…"; written into every event.
    pub device_id: String,
}

/// One finished card. Synced between devices as a line of JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: String,
    pub device: String,
    /// `phrase_uid` of the phrase: the same on every device.
    pub phrase: String,
    pub dir: Direction,
    pub ts: DateTime<Utc>,
    pub outcome: Outcome,
    #[serde(default)]
    pub answer: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StudyCard {
    pub phrase_id: i64,
    pub clip_id: i64,
    pub clip_file: String,
    pub text_pt: String,
    pub text_ru: String,
    pub alt_ru: Vec<String>,
    pub notes: String,
    pub cefr: String,
    pub source_title: String,
    pub is_new: bool,
    pub difficult: bool,
    pub reps: i64,
    pub lapses: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub phrases: i64,
    pub unseen: i64,
    pub learning: i64,
    pub review: i64,
    pub difficult: i64,
    pub due_now: i64,
    pub reviewed_today: i64,
    pub new_today: i64,
}

pub struct NewPhrase<'a> {
    pub text_pt: &'a str,
    pub text_ru: &'a str,
    pub alt_ru: &'a [String],
    pub notes: &'a str,
    pub cefr: &'a str,
    pub usefulness: i64,
}

pub struct NewClip<'a> {
    pub file: &'a str,
    pub source_url: &'a str,
    pub source_title: &'a str,
    pub start_ms: i64,
    pub end_ms: i64,
    pub subs_pt: &'a str,
}

pub struct SourceRow {
    pub video_id: String,
    pub url: String,
    pub channel: String,
    pub title: String,
    pub kind: String,
    pub duration: f64,
    pub subs_file: Option<String>,
    pub status: String,
}

/// Lowercase, strip punctuation, collapse whitespace. Used to dedupe phrases
/// and to compare answers.
pub fn normalize(text: &str) -> String {
    let lowered: String = text
        .to_lowercase()
        .replace('ё', "е")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Stable phrase id shared by all devices: FNV-1a 64 of the normalized text.
pub fn phrase_uid(text_pt: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in normalize(text_pt).bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Unique across devices: device, time and phrase.
fn event_id(device: &str, ts: DateTime<Utc>, uid: &str) -> String {
    format!("{device}-{}-{uid}", ts.timestamp_nanos_opt().unwrap_or_default())
}

fn new_device_id() -> String {
    let prefix = if cfg!(target_os = "android") { "phone" } else { "desktop" };
    let seed = format!(
        "{:?}{}{}",
        std::time::SystemTime::now(),
        std::process::id(),
        std::env::var("COMPUTERNAME").unwrap_or_default()
    );
    format!("{prefix}-{}", &phrase_uid(&seed)[..10])
}

fn start_of_local_day() -> DateTime<Utc> {
    let now = Local::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|d| d.and_local_timezone(Local).single())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| Utc::now())
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        let has_variant: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('sources') WHERE name = 'variant'")?
            .exists([])?;
        if !has_variant {
            // pt-PT | pt-BR | mixed | not-pt; NULL = not checked yet.
            conn.execute_batch("ALTER TABLE sources ADD COLUMN variant TEXT")?;
        }
        let has_uid: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('phrases') WHERE name = 'uid'")?
            .exists([])?;
        if !has_uid {
            conn.execute_batch("ALTER TABLE phrases ADD COLUMN uid TEXT")?;
            let rows: Vec<(i64, String)> = conn
                .prepare("SELECT id, text_pt FROM phrases")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (id, text) in rows {
                conn.execute("UPDATE phrases SET uid = ?2 WHERE id = ?1", params![id, phrase_uid(&text)])?;
            }
        }
        conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS phrases_uid ON phrases(uid)")?;
        let device_id = match conn
            .query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| r.get::<_, String>(0))
            .optional()?
        {
            Some(id) => id,
            None => {
                let id = new_device_id();
                conn.execute("INSERT INTO meta (key, value) VALUES ('device_id', ?1)", [&id])?;
                id
            }
        };
        let db = Self { conn, device_id };
        db.migrate_reviews_to_events()?;
        Ok(db)
    }

    /// Progress recorded before the event log existed lives only in `reviews`
    /// and `attempts`. Rebuild one event per finished card from the attempts.
    fn migrate_reviews_to_events(&self) -> Result<()> {
        let events: i64 = self.conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        let reviews: i64 = self.conn.query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get(0))?;
        if events > 0 || reviews == 0 {
            return Ok(());
        }
        let rows: Vec<(i64, i64, DateTime<Utc>, i64, String, String)> = self
            .conn
            .prepare(
                "SELECT a.phrase_id, a.direction, a.ts, a.attempt_no, a.verdict, a.answer
                 FROM attempts a JOIN reviews r ON r.phrase_id = a.phrase_id AND r.direction = a.direction
                 ORDER BY a.phrase_id, a.direction, a.ts",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<Result<_, _>>()?;
        // A card = attempt 1, optionally followed by attempt 2 within half an hour.
        let mut i = 0;
        while i < rows.len() {
            let (pid, dir, ts1, _, v1, a1) = rows[i].clone();
            let first_ok = matches!(v1.as_str(), "correct" | "partial");
            let second = rows
                .get(i + 1)
                .filter(|n| !first_ok && n.0 == pid && n.1 == dir && n.3 == 2 && (n.2 - ts1).num_minutes() < 30);
            let (outcome, ts, answer) = match (v1.as_str(), second) {
                ("correct", _) => (Outcome::First, ts1, a1),
                ("partial", _) => (Outcome::Second, ts1, a1),
                (_, Some(n)) => {
                    let o = if n.4 == "wrong" { Outcome::Failed } else { Outcome::Second };
                    (o, n.2, format!("{a1} → {}", n.5))
                }
                _ => (Outcome::Failed, ts1, a1),
            };
            i += if second.is_some() { 2 } else { 1 };
            let uid: String = self.conn.query_row("SELECT uid FROM phrases WHERE id = ?1", [pid], |r| r.get(0))?;
            self.insert_event(&Event {
                id: event_id(&self.device_id, ts, &uid),
                device: self.device_id.clone(),
                phrase: uid,
                dir: Direction::from_i64(dir),
                ts,
                outcome,
                answer,
            })?;
        }
        self.recompute_all()?;
        Ok(())
    }

    // ---------- harvesting ----------

    pub fn phrase_exists(&self, text_pt: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM phrases WHERE norm_pt = ?1", [normalize(text_pt)], |r| r.get(0))
            .optional()?)
    }

    pub fn insert_phrase(&self, p: &NewPhrase) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO phrases (text_pt, norm_pt, text_ru, alt_ru, notes, cefr, usefulness, created_at, uid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                p.text_pt,
                normalize(p.text_pt),
                p.text_ru,
                serde_json::to_string(p.alt_ru)?,
                p.notes,
                p.cefr,
                p.usefulness,
                Utc::now(),
                phrase_uid(p.text_pt)
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_clip(&self, phrase_id: i64, c: &NewClip) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO clips (phrase_id, file, source_url, source_title, start_ms, end_ms, subs_pt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![phrase_id, c.file, c.source_url, c.source_title, c.start_ms, c.end_ms, c.subs_pt],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn known_norms(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT norm_pt FROM phrases")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn upsert_source(&self, s: &SourceRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sources (video_id, url, channel, title, kind, duration, subs_file, status, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(video_id) DO UPDATE SET url=?2, channel=?3, title=?4, kind=?5, duration=?6,
                 subs_file=?7, status=?8, updated_at=?9",
            params![s.video_id, s.url, s.channel, s.title, s.kind, s.duration, s.subs_file, s.status, Utc::now()],
        )?;
        Ok(())
    }

    pub fn source_status(&self, video_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT status FROM sources WHERE video_id = ?1", [video_id], |r| r.get(0))
            .optional()?)
    }

    pub fn set_source_status(&self, video_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sources SET status = ?2, updated_at = ?3 WHERE video_id = ?1",
            params![video_id, status, Utc::now()],
        )?;
        Ok(())
    }

    /// Sources usable for mining. `only_pt_pt`: only videos verified as
    /// European Portuguese; otherwise also the not-yet-checked ones.
    pub fn sources_with_subs(&self, only_pt_pt: bool) -> Result<Vec<SourceRow>> {
        self.query_sources(if only_pt_pt {
            "variant = 'pt-PT'"
        } else {
            "(variant IS NULL OR variant = 'pt-PT')"
        })
    }

    pub fn sources_unchecked(&self) -> Result<Vec<SourceRow>> {
        self.query_sources("variant IS NULL")
    }

    pub fn set_source_variant(&self, video_id: &str, variant: &str) -> Result<()> {
        self.conn.execute("UPDATE sources SET variant = ?2 WHERE video_id = ?1", params![video_id, variant])?;
        Ok(())
    }

    /// Removes clips cut from this source and phrases left without clips.
    /// Returns clip file names to delete from disk.
    pub fn remove_source_clips(&self, url: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT file FROM clips WHERE source_url = ?1")?;
        let files: Vec<String> = st.query_map([url], |r| r.get(0))?.collect::<Result<_, _>>()?;
        self.conn.execute("DELETE FROM clips WHERE source_url = ?1", [url])?;
        let orphan = "SELECT id FROM phrases WHERE id NOT IN (SELECT phrase_id FROM clips)";
        self.conn.execute(&format!("DELETE FROM reviews WHERE phrase_id IN ({orphan})"), [])?;
        self.conn.execute(&format!("DELETE FROM attempts WHERE phrase_id IN ({orphan})"), [])?;
        self.conn.execute(&format!("DELETE FROM phrases WHERE id IN ({orphan})"), [])?;
        Ok(files)
    }

    fn query_sources(&self, filter: &str) -> Result<Vec<SourceRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT video_id, url, channel, title, kind, duration, subs_file, status
             FROM sources WHERE subs_file IS NOT NULL AND status != 'skip' AND {filter}"
        ))?;
        let rows = st.query_map([], |r| {
            Ok(SourceRow {
                video_id: r.get(0)?,
                url: r.get(1)?,
                channel: r.get(2)?,
                title: r.get(3)?,
                kind: r.get(4)?,
                duration: r.get(5)?,
                subs_file: r.get(6)?,
                status: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---------- training ----------

    fn card_from_row(r: &rusqlite::Row) -> rusqlite::Result<StudyCard> {
        let alt: String = r.get(5)?;
        Ok(StudyCard {
            phrase_id: r.get(0)?,
            clip_id: r.get(1)?,
            clip_file: r.get(2)?,
            text_pt: r.get(3)?,
            text_ru: r.get(4)?,
            alt_ru: serde_json::from_str(&alt).unwrap_or_default(),
            notes: r.get(6)?,
            cefr: r.get(7)?,
            source_title: r.get(8)?,
            is_new: r.get::<_, Option<i64>>(9)?.is_none(),
            difficult: r.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
            reps: r.get::<_, Option<i64>>(11)?.unwrap_or(0),
            lapses: r.get::<_, Option<i64>>(12)?.unwrap_or(0),
        })
    }

    const CARD_SELECT: &'static str = "
        SELECT p.id,
               (SELECT c.id FROM clips c WHERE c.phrase_id = p.id ORDER BY c.shown, c.id LIMIT 1) AS clip_id,
               (SELECT c.file FROM clips c WHERE c.phrase_id = p.id ORDER BY c.shown, c.id LIMIT 1),
               p.text_pt, p.text_ru, p.alt_ru, p.notes, p.cefr,
               (SELECT c.source_title FROM clips c WHERE c.phrase_id = p.id ORDER BY c.shown, c.id LIMIT 1),
               r.state, r.difficult, r.reps, r.lapses
        FROM phrases p
        LEFT JOIN reviews r ON r.phrase_id = p.id AND r.direction = ?1";

    /// Next card to study: due (re)learning first, then due reviews
    /// (difficult first), then new phrases within the daily limit.
    pub fn next_card(&self, dir: Direction, now: DateTime<Utc>, new_per_day: u32) -> Result<Option<StudyCard>> {
        let due_sql = format!(
            "{} WHERE r.due <= ?2 AND clip_id IS NOT NULL
             ORDER BY CASE WHEN r.state IN (1, 3) THEN 0 ELSE 1 END, r.difficult DESC, r.due LIMIT 1",
            Self::CARD_SELECT
        );
        if let Some(c) = self
            .conn
            .query_row(&due_sql, params![dir as i64, now], Self::card_from_row)
            .optional()?
        {
            return Ok(Some(c));
        }
        if self.new_today(dir)? >= new_per_day as i64 {
            return Ok(None);
        }
        let new_sql = format!(
            "{} WHERE r.phrase_id IS NULL AND clip_id IS NOT NULL
             ORDER BY p.usefulness DESC, p.id LIMIT 1",
            Self::CARD_SELECT
        );
        Ok(self
            .conn
            .query_row(&new_sql, params![dir as i64], Self::card_from_row)
            .optional()?)
    }

    pub fn card_by_phrase(&self, dir: Direction, phrase_id: i64) -> Result<Option<StudyCard>> {
        let sql = format!("{} WHERE p.id = ?2", Self::CARD_SELECT);
        Ok(self
            .conn
            .query_row(&sql, params![dir as i64, phrase_id], Self::card_from_row)
            .optional()?)
    }

    pub fn new_today(&self, dir: Direction) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM reviews WHERE direction = ?1 AND first_review >= ?2",
            params![dir as i64, start_of_local_day()],
            |r| r.get(0),
        )?)
    }

    /// Earliest due time among phrases that are already being studied.
    pub fn earliest_due(&self, dir: Direction) -> Result<Option<DateTime<Utc>>> {
        Ok(self.conn.query_row(
            "SELECT MIN(due) FROM reviews WHERE direction = ?1",
            [dir as i64],
            |r| r.get::<_, Option<DateTime<Utc>>>(0),
        )?)
    }

    pub fn mark_clip_shown(&self, clip_id: i64) -> Result<()> {
        self.conn.execute("UPDATE clips SET shown = shown + 1 WHERE id = ?1", [clip_id])?;
        Ok(())
    }

    pub fn log_attempt(
        &self,
        phrase_id: i64,
        dir: Direction,
        attempt_no: i64,
        answer: &str,
        verdict: &str,
        feedback: &str,
        latency_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO attempts (phrase_id, direction, ts, attempt_no, answer, verdict, feedback, latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![phrase_id, dir as i64, Utc::now(), attempt_no, answer, verdict, feedback, latency_ms],
        )?;
        Ok(())
    }

    /// Records how a card ended and recomputes the phrase's schedule.
    /// Returns when the phrase is due next.
    pub fn record_outcome(
        &self,
        phrase_id: i64,
        dir: Direction,
        outcome: Outcome,
        answer: &str,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>> {
        let uid: String = self.conn.query_row("SELECT uid FROM phrases WHERE id = ?1", [phrase_id], |r| r.get(0))?;
        self.insert_event(&Event {
            id: event_id(&self.device_id, now, &uid),
            device: self.device_id.clone(),
            phrase: uid.clone(),
            dir,
            ts: now,
            outcome,
            answer: answer.to_string(),
        })?;
        Ok(self.recompute(&uid, dir)?.unwrap_or(now))
    }

    /// Adds an event (own or from another device). False if it was already known.
    pub fn insert_event(&self, e: &Event) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO events (id, device, phrase_uid, direction, ts, outcome, answer)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![e.id, e.device, e.phrase, e.dir as i64, e.ts, e.outcome.as_str(), e.answer],
        )?;
        Ok(n > 0)
    }

    pub fn events_of_device(&self, device: &str) -> Result<Vec<Event>> {
        let mut st = self.conn.prepare(
            "SELECT id, device, phrase_uid, direction, ts, outcome, answer FROM events
             WHERE device = ?1 ORDER BY ts, id",
        )?;
        let rows = st.query_map([device], |r| {
            Ok(Event {
                id: r.get(0)?,
                device: r.get(1)?,
                phrase: r.get(2)?,
                dir: Direction::from_i64(r.get(3)?),
                ts: r.get(4)?,
                outcome: Outcome::parse(&r.get::<_, String>(5)?).unwrap_or(Outcome::Failed),
                answer: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Replays all events of a phrase in time order (all devices) into the
    /// `reviews` cache. Deterministic: every device computes the same schedule.
    pub fn recompute(&self, uid: &str, dir: Direction) -> Result<Option<DateTime<Utc>>> {
        let Some(phrase_id) = self
            .conn
            .query_row("SELECT id FROM phrases WHERE uid = ?1", [uid], |r| r.get::<_, i64>(0))
            .optional()?
        else {
            // Content not here yet (synced later): the events wait.
            return Ok(None);
        };
        let events: Vec<(DateTime<Utc>, String)> = self
            .conn
            .prepare("SELECT ts, outcome FROM events WHERE phrase_uid = ?1 AND direction = ?2 ORDER BY ts, id")?
            .query_map(params![uid, dir as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        if events.is_empty() {
            self.conn
                .execute("DELETE FROM reviews WHERE phrase_id = ?1 AND direction = ?2", params![phrase_id, dir as i64])?;
            return Ok(None);
        }
        let mut card = rs_fsrs::Card::new();
        let (mut difficult, mut streak) = (false, 0);
        for (ts, outcome) in &events {
            let rating = Outcome::parse(outcome).unwrap_or(Outcome::Failed).rating();
            card = srs::schedule(card, *ts, rating);
            (difficult, streak) = srs::difficult_flag(difficult, streak, card.lapses, rating);
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO reviews (phrase_id, direction, due, stability, difficulty, elapsed_days,
                 scheduled_days, reps, lapses, state, last_review, first_review, difficult, streak)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                phrase_id,
                dir as i64,
                card.due,
                card.stability,
                card.difficulty,
                card.elapsed_days,
                card.scheduled_days,
                card.reps,
                card.lapses,
                card.state as i64,
                card.last_review,
                events[0].0,
                difficult as i64,
                streak
            ],
        )?;
        Ok(Some(card.due))
    }

    pub fn recompute_all(&self) -> Result<()> {
        let keys: Vec<(String, i64)> = self
            .conn
            .prepare("SELECT DISTINCT phrase_uid, direction FROM events")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (uid, dir) in keys {
            self.recompute(&uid, Direction::from_i64(dir))?;
        }
        Ok(())
    }

    pub fn stats(&self, dir: Direction) -> Result<Stats> {
        let d = dir as i64;
        let q = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [d], |r| r.get(0))?) };
        let mut s = Stats {
            phrases: self.conn.query_row("SELECT COUNT(*) FROM phrases", [], |r| r.get(0))?,
            learning: q("SELECT COUNT(*) FROM reviews WHERE direction = ?1 AND state IN (1, 3)")?,
            review: q("SELECT COUNT(*) FROM reviews WHERE direction = ?1 AND state = 2")?,
            difficult: q("SELECT COUNT(*) FROM reviews WHERE direction = ?1 AND difficult = 1")?,
            ..Default::default()
        };
        s.unseen = s.phrases - q("SELECT COUNT(*) FROM reviews WHERE direction = ?1")?;
        s.due_now = self.conn.query_row(
            "SELECT COUNT(*) FROM reviews WHERE direction = ?1 AND due <= ?2",
            params![d, Utc::now()],
            |r| r.get(0),
        )?;
        s.reviewed_today = self.conn.query_row(
            "SELECT COUNT(DISTINCT phrase_id) FROM attempts WHERE direction = ?1 AND ts >= ?2",
            params![d, start_of_local_day()],
            |r| r.get(0),
        )?;
        s.new_today = self.new_today(dir)?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srs::Outcome;

    fn temp_db() -> Db {
        let path = std::env::temp_dir().join(format!("teachme-test-{}-{}.db", std::process::id(), rand_suffix()));
        let _ = std::fs::remove_file(&path);
        Db::open(&path).unwrap()
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    fn add(db: &Db, pt: &str, usefulness: i64) -> i64 {
        let id = db
            .insert_phrase(&NewPhrase { text_pt: pt, text_ru: "ru", alt_ru: &[], notes: "", cefr: "A2", usefulness })
            .unwrap();
        db.insert_clip(id, &NewClip { file: "x.mp4", source_url: "u", source_title: "t", start_ms: 0, end_ms: 1000, subs_pt: pt })
            .unwrap();
        id
    }

    #[test]
    fn new_cards_by_usefulness_then_failed_comes_back_first() {
        let db = temp_db();
        let a = add(&db, "Está bem.", 3);
        let b = add(&db, "Não faz mal.", 5);
        let now = Utc::now();
        let first = db.next_card(Direction::PtToRu, now, 10).unwrap().unwrap();
        assert_eq!(first.phrase_id, b);
        assert!(first.is_new);

        db.record_outcome(b, Direction::PtToRu, Outcome::Failed, "", now).unwrap();
        db.record_outcome(a, Direction::PtToRu, Outcome::First, "", now).unwrap();
        // Later: the failed phrase is due before anything else and is marked difficult.
        let later = now + chrono::Duration::minutes(20);
        let next = db.next_card(Direction::PtToRu, later, 10).unwrap().unwrap();
        assert_eq!(next.phrase_id, b);
        assert!(next.difficult);
        assert_eq!(db.new_today(Direction::PtToRu).unwrap(), 2);
    }

    /// Two devices that exchange their events end up with the same schedule,
    /// whatever order the events arrive in.
    #[test]
    fn devices_converge_after_exchanging_events() {
        let pc = temp_db();
        let phone = temp_db();
        assert_ne!(pc.device_id, phone.device_id);
        for db in [&pc, &phone] {
            add(db, "Está bem.", 3);
            add(db, "Não faz mal.", 5);
        }
        let t0 = Utc::now() - chrono::Duration::days(3);
        let pid = |db: &Db, pt: &str| db.phrase_exists(pt).unwrap().unwrap();
        pc.record_outcome(pid(&pc, "Está bem."), Direction::PtToRu, Outcome::Failed, "", t0).unwrap();
        phone
            .record_outcome(pid(&phone, "Está bem."), Direction::PtToRu, Outcome::Second, "", t0 + chrono::Duration::hours(1))
            .unwrap();
        pc.record_outcome(pid(&pc, "Está bem."), Direction::PtToRu, Outcome::First, "", t0 + chrono::Duration::days(1))
            .unwrap();
        phone
            .record_outcome(pid(&phone, "Não faz mal."), Direction::PtToRu, Outcome::First, "", t0 + chrono::Duration::days(2))
            .unwrap();

        // Exchange (as sync would do through the shared folder).
        for e in pc.events_of_device(&pc.device_id.clone()).unwrap() {
            phone.insert_event(&e).unwrap();
        }
        for e in phone.events_of_device(&phone.device_id.clone()).unwrap() {
            assert!(pc.insert_event(&e).unwrap());
            assert!(!pc.insert_event(&e).unwrap(), "duplicates are ignored");
        }
        pc.recompute_all().unwrap();
        phone.recompute_all().unwrap();

        let schedule = |db: &Db| -> Vec<(String, String, i64, i64)> {
            db.conn
                .prepare(
                    "SELECT p.uid, r.due, r.reps, r.difficult FROM reviews r JOIN phrases p ON p.id = r.phrase_id ORDER BY p.uid",
                )
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let a = schedule(&pc);
        assert_eq!(a.len(), 2);
        assert_eq!(a, schedule(&phone));
        // Three answers on "Está bem.", from both devices.
        assert!(a.iter().any(|r| r.2 == 3));
    }

    #[test]
    fn daily_new_limit() {
        let db = temp_db();
        let a = add(&db, "Bom dia.", 3);
        add(&db, "Boa noite.", 3);
        db.record_outcome(a, Direction::PtToRu, Outcome::First, "", Utc::now()).unwrap();
        assert!(db.next_card(Direction::PtToRu, Utc::now(), 1).unwrap().is_none());
    }

    #[test]
    fn duplicate_phrases_are_detected() {
        let db = temp_db();
        add(&db, "Está bem!", 3);
        assert!(db.phrase_exists("está bem").unwrap().is_some());
    }
}
