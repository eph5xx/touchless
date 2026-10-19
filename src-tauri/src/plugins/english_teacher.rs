//! After each English dictation, points out one grammar or usage mistake and
//! explains it, or says the text sounds natural.

use serde::Deserialize;
use serde_json::json;

use crate::plugin_api::{
    Block, BoxFuture, Card, Dictation, Instrument, JsonRequest, Manifest, Plugin, Tone, Tools,
};

pub struct EnglishTeacher;

static MANIFEST: Manifest = Manifest {
    id: "english_teacher",
    name: "English Teacher",
    description: "After each English dictation, shows one grammar correction with a short explanation. Your words are still pasted as you said them.",
    needs: &[Instrument::Llm],
};

/// Shorter dictations rarely have grammar worth teaching.
const MIN_WORDS: usize = 3;

const INSTRUCTIONS: &str = "You are a friendly English teacher. A non-native speaker dictated the text below by voice. \
It was transcribed automatically and has already been sent as is.
Ignore punctuation, capitalization, filler words, false starts and likely speech-to-text errors.
Set is_english to false if the text is mostly not English.
Otherwise find the single most meaningful grammar or usage mistake: tense, agreement, articles, prepositions, \
word order, or a word a native speaker would not use there. Skip matters of style or taste.
If there is such a mistake, set has_correction to true. Put the shortest phrase that contains it in original, \
copied exactly from the text, and the fixed phrase in corrected. In explanation, write one plain sentence \
of at most 25 words that tells the speaker why.
If the text sounds natural, set has_correction to false and leave original, corrected and explanation empty.";

#[derive(Debug, Deserialize)]
struct Verdict {
    is_english: bool,
    has_correction: bool,
    original: String,
    corrected: String,
    explanation: String,
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["is_english", "has_correction", "original", "corrected", "explanation"],
        "properties": {
            "is_english": { "type": "boolean" },
            "has_correction": { "type": "boolean" },
            "original": { "type": "string" },
            "corrected": { "type": "string" },
            "explanation": { "type": "string" }
        }
    })
}

fn is_english(code: &str) -> bool {
    let code = code.trim().to_ascii_lowercase();
    code == "en" || code == "eng" || code.starts_with("en-") || code.starts_with("en_")
}

fn card(verdict: Verdict) -> Option<Card> {
    if !verdict.is_english {
        return None;
    }
    let original = verdict.original.trim();
    let corrected = verdict.corrected.trim();
    let explanation = verdict.explanation.trim();
    let corrects_something = verdict.has_correction
        && !original.is_empty()
        && !corrected.is_empty()
        && original != corrected;
    if !corrects_something {
        return Some(Card {
            tone: Tone::Success,
            blocks: vec![Block::Text {
                text: "Sounds natural".into(),
            }],
        });
    }

    let mut blocks = vec![Block::Change {
        before: original.to_owned(),
        after: corrected.to_owned(),
    }];
    if !explanation.is_empty() {
        blocks.push(Block::Text {
            text: explanation.to_owned(),
        });
    }
    Some(Card {
        tone: Tone::Tip,
        blocks,
    })
}

impl Plugin for EnglishTeacher {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn wants(&self, dictation: &Dictation) -> bool {
        let long_enough = dictation.text.split_whitespace().count() >= MIN_WORDS;
        // With no detected language the model decides whether the text is English.
        let english = dictation.language.as_deref().is_none_or(is_english);
        long_enough && english
    }

    fn on_dictation<'a>(
        &'a self,
        dictation: &'a Dictation,
        tools: &'a Tools<'a>,
    ) -> BoxFuture<'a, Result<Option<Card>, String>> {
        Box::pin(async move {
            let answer = tools
                .llm
                .json(JsonRequest {
                    instructions: INSTRUCTIONS.into(),
                    input: dictation.text.clone(),
                    schema_name: "grammar_check".into(),
                    schema: schema(),
                })
                .await?;
            let verdict: Verdict = serde_json::from_value(answer)
                .map_err(|error| format!("unexpected answer from the model: {error}"))?;
            Ok(card(verdict))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_api::Llm;

    struct FakeLlm(Result<serde_json::Value, String>);

    impl Llm for FakeLlm {
        fn json(&self, request: JsonRequest) -> BoxFuture<'_, Result<serde_json::Value, String>> {
            assert_eq!(request.schema_name, "grammar_check");
            let answer = self.0.clone();
            Box::pin(async move { answer })
        }
    }

    fn dictation(text: &str, language: Option<&str>) -> Dictation {
        Dictation {
            transcript_id: 1,
            text: text.into(),
            language: language.map(Into::into),
        }
    }

    async fn check(answer: serde_json::Value) -> Result<Option<Card>, String> {
        let llm = FakeLlm(Ok(answer));
        let tools = Tools { llm: &llm };
        let dictation = dictation("I have went there yesterday", Some("eng"));
        EnglishTeacher.on_dictation(&dictation, &tools).await
    }

    fn verdict(
        is_english: bool,
        has_correction: bool,
        original: &str,
        corrected: &str,
    ) -> serde_json::Value {
        json!({
            "is_english": is_english,
            "has_correction": has_correction,
            "original": original,
            "corrected": corrected,
            "explanation": if has_correction { " Use the past simple with \"yesterday\". " } else { "" },
        })
    }

    #[test]
    fn wants_english_or_unknown_dictations_of_a_few_words() {
        let teacher = EnglishTeacher;
        assert!(teacher.wants(&dictation("I goes to work", Some("eng"))));
        assert!(teacher.wants(&dictation("I goes to work", Some("en"))));
        assert!(teacher.wants(&dictation("I goes to work", Some("en-US"))));
        assert!(teacher.wants(&dictation("I goes to work", None)));
        assert!(!teacher.wants(&dictation("Я иду на работу", Some("rus"))));
        assert!(!teacher.wants(&dictation("Thanks a", Some("eng"))));
    }

    #[tokio::test]
    async fn shows_one_correction_with_its_explanation() {
        let card = check(verdict(true, true, "have went", "went"))
            .await
            .unwrap();
        assert_eq!(
            card,
            Some(Card {
                tone: Tone::Tip,
                blocks: vec![
                    Block::Change {
                        before: "have went".into(),
                        after: "went".into()
                    },
                    Block::Text {
                        text: "Use the past simple with \"yesterday\".".into()
                    },
                ],
            })
        );
    }

    #[tokio::test]
    async fn says_sounds_natural_when_nothing_to_fix() {
        let natural = Some(Card {
            tone: Tone::Success,
            blocks: vec![Block::Text {
                text: "Sounds natural".into(),
            }],
        });
        assert_eq!(check(verdict(true, false, "", "")).await.unwrap(), natural);
        // A "correction" that changes nothing is not worth showing.
        assert_eq!(
            check(verdict(true, true, "went", "went")).await.unwrap(),
            natural
        );
    }

    #[tokio::test]
    async fn stays_quiet_for_other_languages() {
        assert_eq!(check(verdict(false, false, "", "")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn reports_model_errors() {
        assert!(check(json!({ "is_english": true })).await.is_err());

        let llm = FakeLlm(Err("OpenAI rejected the key".into()));
        let tools = Tools { llm: &llm };
        let dictation = dictation("I have went there", None);
        let result = EnglishTeacher.on_dictation(&dictation, &tools).await;
        assert_eq!(result, Err("OpenAI rejected the key".into()));
    }
}
