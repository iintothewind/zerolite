//! Question frames on `/ws/chat`: the browser's answer card for `ask_user`
//! and `poll`.
//!
//! Server → browser: `{"type":"question_request","request_id","question",
//! "choices","multi_select","free_text","timeout_secs"}`. Browser → server:
//! `{"type":"question_response","request_id","answers"}`, where `answers` is a
//! non-empty array of non-blank strings for an answer and anything else for a
//! dismissal. Both chat paths speak this contract: the operator chat parks
//! the question in-process (`WsApprovalChannel`), the scoped chat relays it
//! as an ACP `elicitation/create` answer (`chat_core`).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::oneshot;

/// `None` = dismissed.
pub type QuestionReply = Option<Vec<String>>;

/// Questions waiting for the viewer, keyed by `request_id`.
pub type PendingQuestions =
    Arc<parking_lot::Mutex<HashMap<String, oneshot::Sender<QuestionReply>>>>;

/// Construct an empty pending-questions registry for a fresh connection.
pub fn new_pending_questions() -> PendingQuestions {
    Arc::new(parking_lot::Mutex::new(HashMap::new()))
}

/// The `question_request` frame. `free_text` tells the browser whether an
/// answer outside `choices` can be returned; `timeout_secs: None` means the
/// gateway does not know the deadline.
pub fn question_request_frame(
    request_id: &str,
    question: &str,
    choices: &[String],
    multi_select: bool,
    free_text: bool,
    timeout_secs: Option<u64>,
) -> Value {
    json!({
        "type": "question_request",
        "request_id": request_id,
        "question": question,
        "choices": choices,
        "multi_select": multi_select,
        "free_text": free_text,
        "timeout_secs": timeout_secs,
    })
}

/// Read a `question_response` frame. `None` when `request_id` is missing or
/// empty.
pub fn question_response(frame: &Value) -> Option<(&str, QuestionReply)> {
    let request_id = frame
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let answers: Vec<String> = frame
        .get("answers")
        .and_then(Value::as_array)
        .map(|answers| {
            answers
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|answer| !answer.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some((request_id, (!answers.is_empty()).then_some(answers)))
}

/// Deliver a `question_response` to its parked question. `false` = malformed
/// frame.
pub fn resolve_question(pending: &PendingQuestions, frame: &Value) -> bool {
    let Some((request_id, reply)) = question_response(frame) else {
        return false;
    };
    let parked = pending.lock().remove(request_id);
    match parked {
        Some(tx) => {
            let _ = tx.send(reply);
        }
        None => {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"request_id": request_id})),
                "question_response with no matching pending question"
            );
        }
    }
    true
}

/// A choice question read back from an ACP `elicitation/create` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elicited {
    pub question: String,
    pub choices: Vec<String>,
    pub multi_select: bool,
}

/// Inverse of `zeroclaw_api::elicitation::single_select_schema` /
/// `multi_select_schema`. `None` for any other shape, a different session,
/// or an empty choice list.
pub fn parse_elicitation(params: &Value, session_id: &str) -> Option<Elicited> {
    if params.get("sessionId").and_then(Value::as_str) != Some(session_id)
        || params.get("mode").and_then(Value::as_str) != Some("form")
    {
        return None;
    }
    let question = params.get("message").and_then(Value::as_str)?.to_string();
    let properties = params.get("requestedSchema")?.get("properties")?;
    let (entries, multi_select) = if let Some(entries) = properties
        .pointer("/choice/oneOf")
        .and_then(Value::as_array)
    {
        (entries, false)
    } else {
        (
            properties
                .pointer("/choices/items/anyOf")
                .and_then(Value::as_array)?,
            true,
        )
    };
    if entries.is_empty() {
        return None;
    }
    let choices = entries
        .iter()
        .enumerate()
        .map(|(position, entry)| {
            let expected = format!("choice-{position}");
            (entry.get("const").and_then(Value::as_str) == Some(expected.as_str()))
                .then(|| {
                    entry
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .flatten()
        })
        .collect::<Option<Vec<String>>>()?;
    Some(Elicited {
        question,
        choices,
        multi_select,
    })
}

/// The `elicitation/create` result for the viewer's reply: the answered
/// choices as `choice-<i>` indices, or `decline` when nothing maps.
pub fn elicitation_answer(elicited: &Elicited, reply: &QuestionReply) -> Value {
    let mut picked: Vec<usize> = Vec::new();
    for answer in reply.iter().flatten() {
        if let Some(index) = elicited.choices.iter().position(|choice| choice == answer)
            && !picked.contains(&index)
        {
            picked.push(index);
        }
    }
    let Some(first) = picked.first() else {
        return json!({"action": "decline"});
    };
    if elicited.multi_select {
        let choices: Vec<String> = picked.iter().map(|i| format!("choice-{i}")).collect();
        json!({"action": "accept", "content": {"choices": choices}})
    } else {
        json!({"action": "accept", "content": {"choice": format!("choice-{first}")}})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::elicitation::{
        decode_multi_select_accept, decode_single_select_accept, multi_select_schema,
        single_select_schema,
    };

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn params(schema: Value) -> Value {
        json!({
            "sessionId": "s-1",
            "mode": "form",
            "message": "Which color?",
            "requestedSchema": schema,
        })
    }

    #[test]
    fn question_response_trims_and_treats_blank_answers_as_dismissed() {
        let reply = |answers: Value| {
            question_response(&json!({"request_id": "q-1", "answers": answers}))
                .map(|(id, reply)| (id.to_string(), reply))
        };
        assert_eq!(
            reply(json!([" B ", ""])),
            Some(("q-1".to_string(), Some(strings(&["B"]))))
        );
        assert_eq!(reply(Value::Null), Some(("q-1".to_string(), None)));
        assert_eq!(reply(json!([])), Some(("q-1".to_string(), None)));
        assert_eq!(reply(json!(["  "])), Some(("q-1".to_string(), None)));
        assert!(question_response(&json!({"answers": ["B"]})).is_none());
    }

    #[test]
    fn parse_elicitation_reads_what_the_schema_helpers_build() {
        let choices = strings(&["Red", "Green", "Blue"]);

        let single = parse_elicitation(&params(single_select_schema(&choices)), "s-1")
            .expect("a single-select elicitation");
        assert_eq!(single.question, "Which color?");
        assert_eq!(single.choices, choices);
        assert!(!single.multi_select);

        let multi = parse_elicitation(&params(multi_select_schema(&choices, 1, 3)), "s-1")
            .expect("a multi-select elicitation");
        assert_eq!(multi.choices, choices);
        assert!(multi.multi_select);

        assert!(parse_elicitation(&params(single_select_schema(&choices)), "s-2").is_none());
        assert!(parse_elicitation(&params(single_select_schema(&[])), "s-1").is_none());
    }

    #[test]
    fn elicitation_answer_round_trips_through_the_core_decoders() {
        let choices = strings(&["Red", "Green", "Blue"]);
        let single = Elicited {
            question: "Which?".into(),
            choices: choices.clone(),
            multi_select: false,
        };
        let answer = elicitation_answer(&single, &Some(strings(&["Green"])));
        assert_eq!(answer["action"], "accept");
        assert_eq!(
            decode_single_select_accept(&answer["content"], &choices).unwrap(),
            "Green"
        );

        let multi = Elicited {
            multi_select: true,
            ..single.clone()
        };
        let answer = elicitation_answer(&multi, &Some(strings(&["Blue", "Red", "Blue", "Teal"])));
        assert_eq!(
            decode_multi_select_accept(&answer["content"], &choices).unwrap(),
            strings(&["Blue", "Red"])
        );

        assert_eq!(
            elicitation_answer(&single, &Some(strings(&["Teal"]))),
            json!({"action": "decline"})
        );
        assert_eq!(
            elicitation_answer(&single, &None),
            json!({"action": "decline"})
        );
    }
}
