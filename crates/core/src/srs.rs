use chrono::{DateTime, Utc};
use rs_fsrs::{Card, Parameters, Rating, State, FSRS};
use serde::{Deserialize, Serialize};

/// Training direction. Each direction has its own FSRS state per phrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Watch the Portuguese clip, say the Russian translation.
    PtToRu = 0,
    /// See the Russian phrase, say it in Portuguese.
    RuToPt = 1,
}

/// How a card ended, as reported by the trainer UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Correct on the first attempt (no subtitles), answered quickly.
    FirstFast,
    /// Correct on the first attempt.
    First,
    /// Correct only with subtitles, or partially correct.
    Second,
    /// Failed both attempts.
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::FirstFast => "first_fast",
            Outcome::First => "first",
            Outcome::Second => "second",
            Outcome::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "first_fast" => Outcome::FirstFast,
            "first" => Outcome::First,
            "second" => Outcome::Second,
            "failed" => Outcome::Failed,
            _ => return None,
        })
    }

    pub fn rating(self) -> Rating {
        match self {
            Outcome::FirstFast => Rating::Easy,
            Outcome::First => Rating::Good,
            Outcome::Second => Rating::Hard,
            Outcome::Failed => Rating::Again,
        }
    }
}

impl Direction {
    pub fn from_i64(v: i64) -> Self {
        if v == 1 {
            Direction::RuToPt
        } else {
            Direction::PtToRu
        }
    }
}

pub fn state_from_i64(v: i64) -> State {
    match v {
        1 => State::Learning,
        2 => State::Review,
        3 => State::Relearning,
        _ => State::New,
    }
}

pub fn schedule(card: Card, now: DateTime<Utc>, rating: Rating) -> Card {
    let fsrs = FSRS::new(Parameters {
        enable_fuzz: true,
        ..Default::default()
    });
    fsrs.next(card, now, rating).card
}

/// A phrase becomes "difficult" when it is failed (or has lapsed twice) and
/// stops being difficult after three good answers in a row.
pub fn difficult_flag(difficult: bool, streak: i64, lapses: i32, rating: Rating) -> (bool, i64) {
    match rating {
        Rating::Again => (true, 0),
        Rating::Hard => (difficult || lapses >= 2, streak),
        Rating::Good | Rating::Easy => {
            let streak = streak + 1;
            if difficult && streak >= 3 {
                (false, streak)
            } else {
                (difficult, streak)
            }
        }
    }
}
