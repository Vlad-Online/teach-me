use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;

use crate::db::{normalize, StudyCard};
use crate::llm::Llm;
use crate::openai::Purpose;
use crate::srs::Direction;

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    /// "correct" | "partial" | "wrong"
    pub verdict: String,
    pub feedback: String,
}

const JUDGE_PT_RU: &str = "Ты проверяешь, как ученик (уровень A2–B1) перевёл фразу с европейского португальского на русский.
Оцени только смысл.
Допустимо: синонимы, другой порядок слов, разговорный стиль, пропуск несущественных частиц и междометий.
Ошибка (wrong): другой смысл; потеряно или добавлено отрицание; потеряно слово, меняющее смысл утверждения («не только», «уже», «ещё», «тоже», «даже»); не то лицо или число; неверное ключевое слово.
partial — смысл в основном передан, но упущена или искажена второстепенная деталь (например, оттенок времени).
Разговорные эквиваленты считаются верными: «я не понимаю» ≈ «я не понял», «до следующей недели» ≈ «увидимся на следующей неделе».
Ответ ученика получен распознаванием речи и может содержать ошибки распознавания — не наказывай за них, если понятно, что имелось в виду.
feedback — по-русски, 1–2 коротких предложения: что именно не так и как правильно. Для correct — пустая строка или короткий нюанс.";

const JUDGE_RU_PT: &str = "Ты проверяешь, как ученик (уровень A2–B1) перевёл русскую фразу на европейский португальский (pt-PT).
Оцени смысл и грамматику. Допустимо любое естественное для pt-PT выражение того же смысла, не обязательно совпадающее с эталоном.
Бразильский вариант считай partial и укажи на это.
Ответ ученика получен распознаванием речи — не наказывай за орфографию и мелкие ошибки распознавания.
feedback — по-русски, 1–2 коротких предложения: что не так и как правильно. Для correct — пустая строка или короткий нюанс.";

fn words(s: &str) -> HashSet<String> {
    normalize(s).split(' ').filter(|w| !w.is_empty()).map(String::from).collect()
}

/// Exact (normalized) match against the reference or its alternatives.
fn local_match(answer: &str, refs: &[&str]) -> bool {
    let a = normalize(answer);
    !a.is_empty() && refs.iter().any(|r| normalize(r) == a)
}

/// Rough word-overlap check used when the API is unavailable.
fn overlap_verdict(answer: &str, refs: &[&str]) -> Verdict {
    let a = words(answer);
    let best = refs
        .iter()
        .map(|r| {
            let r = words(r);
            let inter = a.intersection(&r).count() as f64;
            let union = a.union(&r).count().max(1) as f64;
            inter / union
        })
        .fold(0.0, f64::max);
    let verdict = if best >= 0.75 {
        "correct"
    } else if best >= 0.45 {
        "partial"
    } else {
        "wrong"
    };
    Verdict {
        verdict: verdict.into(),
        feedback: "Проверка приблизительная: нет доступа к OpenAI.".into(),
    }
}

pub fn check(api: Option<&Llm>, card: &StudyCard, dir: Direction, answer: &str) -> Result<Verdict> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(Verdict { verdict: "wrong".into(), feedback: String::new() });
    }
    let mut refs: Vec<&str> = vec![];
    match dir {
        Direction::PtToRu => {
            refs.push(&card.text_ru);
            refs.extend(card.alt_ru.iter().map(String::as_str));
        }
        Direction::RuToPt => refs.push(&card.text_pt),
    }
    if local_match(answer, &refs) {
        return Ok(Verdict { verdict: "correct".into(), feedback: String::new() });
    }
    let Some(api) = api else {
        return Ok(overlap_verdict(answer, &refs));
    };
    let (system, user) = match dir {
        Direction::PtToRu => (
            JUDGE_PT_RU,
            json!({
                "original_pt": card.text_pt,
                "reference_ru": card.text_ru,
                "acceptable_ru": card.alt_ru,
                "student_answer_ru": answer,
            }),
        ),
        Direction::RuToPt => (
            JUDGE_RU_PT,
            json!({
                "phrase_ru": card.text_ru,
                "reference_pt": card.text_pt,
                "student_answer_pt": answer,
            }),
        ),
    };
    let schema = json!({
        "type": "object",
        "properties": {
            "verdict": {"type": "string", "enum": ["correct", "partial", "wrong"]},
            "feedback": {"type": "string"}
        },
        "required": ["verdict", "feedback"],
        "additionalProperties": false
    });
    match api.json(Purpose::Judge, system, &user.to_string(), "verdict", schema) {
        Ok(v) => Ok(Verdict {
            verdict: v["verdict"].as_str().unwrap_or("wrong").to_string(),
            feedback: v["feedback"].as_str().unwrap_or_default().to_string(),
        }),
        Err(e) => {
            let mut v = overlap_verdict(answer, &refs);
            v.feedback = format!("Проверка приблизительная: {e}");
            Ok(v)
        }
    }
}
