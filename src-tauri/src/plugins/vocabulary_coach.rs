//! After each English dictation, suggests one word the speaker could have
//! chosen better, with a few words on why.

use serde::Deserialize;
use serde_json::json;

use crate::plugin_api::{
    Block, BoxFuture, Card, Dictation, Instrument, JsonRequest, Manifest, Plugin, Tone, Tools,
};

pub struct VocabularyCoach;

static MANIFEST: Manifest = Manifest {
    // The plugin used to be "English Teacher"; saved settings and history use this id.
    id: "english_teacher",
    name: "Vocabulary Coach",
    description: "After English dictations, suggests one better word in a small card at the top-right. Your words are still pasted as you said them.",
    needs: &[Instrument::Llm],
};

/// Shorter dictations rarely have a word choice worth improving.
const MIN_WORDS: usize = 3;

const INSTRUCTIONS: &str = "You are a vocabulary coach for a non-native English speaker who dictated the text below by voice. \
It was transcribed automatically and has already been sent as is.
Ignore grammar, punctuation, capitalization, filler words, false starts and likely speech-to-text errors.
Set is_english to false if the text is mostly not English.
Otherwise look only at word choice and keep exactly what the speaker meant. Never suggest a word that changes the meaning, \
even slightly, and never guess at a different intent. If you are not sure what the speaker meant, set has_tip to false.
Look for: a word that is almost right but not what a native speaker would say (for example \"reach my friends\" instead of \
\"reach out to my friends\"), a wrong word combination (like \"make a photo\" instead of \"take a photo\"), \
or a vague, overused word (like good, simple, big, do, make, thing) where a more precise everyday word says the same thing better. \
Suggest words the speaker can reuse in daily speech; skip rare, formal or fancy words and changes that are only a matter of taste.
Before answering, put the replacement into the sentence and check that it says the same thing as the original. If not, set has_tip to false.
If there is such a word, set has_tip to true. Put the word or shortest phrase in used, copied exactly from the text, \
and the replacement in better, in the form that fits the sentence. In note, write a fragment of at most 6 words \
saying why the better word fits, for example \"the usual phrase for contacting someone\". Do not define the word. No full sentence, no quotes.
If no word is clearly better, set has_tip to false and leave used, better and note empty.";

#[derive(Debug, Deserialize)]
struct Verdict {
    is_english: bool,
    has_tip: bool,
    used: String,
    better: String,
    note: String,
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["is_english", "has_tip", "used", "better", "note"],
        "properties": {
            "is_english": { "type": "boolean" },
            "has_tip": { "type": "boolean" },
            "used": { "type": "string" },
            "better": { "type": "string" },
            "note": { "type": "string" }
        }
    })
}

fn is_english(code: &str) -> bool {
    let code = code.trim().to_ascii_lowercase();
    code == "en" || code == "eng" || code.starts_with("en-") || code.starts_with("en_")
}

fn card(verdict: Verdict) -> Option<Card> {
    let used = verdict.used.trim();
    let better = verdict.better.trim();
    let note = verdict.note.trim();
    let has_tip = verdict.is_english
        && verdict.has_tip
        && !used.is_empty()
        && !better.is_empty()
        && !used.eq_ignore_ascii_case(better);
    if !has_tip {
        return None;
    }

    let mut blocks = vec![Block::Change {
        before: used.to_owned(),
        after: better.to_owned(),
    }];
    if !note.is_empty() {
        blocks.push(Block::Text {
            text: note.to_owned(),
        });
    }
    Some(Card {
        tone: Tone::Tip,
        blocks,
    })
}

impl Plugin for VocabularyCoach {
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
                    schema_name: "word_tip".into(),
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
            assert_eq!(request.schema_name, "word_tip");
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
        let dictation = dictation("It was a simple chat with friends", Some("eng"));
        VocabularyCoach.on_dictation(&dictation, &tools).await
    }

    fn verdict(
        is_english: bool,
        has_tip: bool,
        used: &str,
        better: &str,
        note: &str,
    ) -> serde_json::Value {
        json!({
            "is_english": is_english,
            "has_tip": has_tip,
            "used": used,
            "better": better,
            "note": note,
        })
    }

    #[test]
    fn wants_english_or_unknown_dictations_of_a_few_words() {
        let coach = VocabularyCoach;
        assert!(coach.wants(&dictation("I goes to work", Some("eng"))));
        assert!(coach.wants(&dictation("I goes to work", Some("en"))));
        assert!(coach.wants(&dictation("I goes to work", Some("en-US"))));
        assert!(coach.wants(&dictation("I goes to work", None)));
        assert!(!coach.wants(&dictation("Я иду на работу", Some("rus"))));
        assert!(!coach.wants(&dictation("Thanks a", Some("eng"))));
    }

    #[tokio::test]
    async fn suggests_one_better_word_with_a_short_note() {
        let card = check(verdict(
            true,
            true,
            " simple ",
            " casual ",
            " relaxed and informal ",
        ))
        .await
        .unwrap();
        assert_eq!(
            card,
            Some(Card {
                tone: Tone::Tip,
                blocks: vec![
                    Block::Change {
                        before: "simple".into(),
                        after: "casual".into()
                    },
                    Block::Text {
                        text: "relaxed and informal".into()
                    },
                ],
            })
        );
    }

    #[tokio::test]
    async fn leaves_out_an_empty_note() {
        let card = check(verdict(true, true, "simple", "casual", ""))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(card.blocks.len(), 1);
    }

    #[tokio::test]
    async fn stays_quiet_without_a_real_tip() {
        assert_eq!(check(verdict(true, false, "", "", "")).await.unwrap(), None);
        assert_eq!(
            check(verdict(true, true, "", "casual", "x")).await.unwrap(),
            None
        );
        // A "tip" that changes nothing is not worth showing.
        assert_eq!(
            check(verdict(true, true, "Simple", "simple", "x"))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn stays_quiet_for_other_languages() {
        assert_eq!(
            check(verdict(false, true, "simple", "casual", "x"))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn reports_model_errors() {
        assert!(check(json!({ "is_english": true })).await.is_err());

        let llm = FakeLlm(Err("OpenAI rejected the key".into()));
        let tools = Tools { llm: &llm };
        let dictation = dictation("It was a simple chat", None);
        let result = VocabularyCoach.on_dictation(&dictation, &tools).await;
        assert_eq!(result, Err("OpenAI rejected the key".into()));
    }
}
