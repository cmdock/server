#![no_main]

use cmdock_server::webhooks::api::{
    normalize_events, normalize_modified_fields, normalize_name, CreateWebhookRequest,
    UpdateWebhookRequest,
};
use libfuzzer_sys::fuzz_target;

fn exercise_normalizers(
    events: &[String],
    modified_fields: Option<&Vec<String>>,
    name: Option<&str>,
) {
    let _ = normalize_name(name);
    let normalized_events = normalize_events(events);
    if let Ok(ref accepted_events) = normalized_events {
        let _ = normalize_modified_fields(accepted_events, modified_fields);
    }
    let _ = normalize_modified_fields(events, modified_fields);
}

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let split_strings: Vec<String> = input
            .split([',', '\n', '\r'])
            .take(16)
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        let field_strings: Vec<String> = input
            .split([' ', '\t', ',', '\n'])
            .take(16)
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        exercise_normalizers(&split_strings, Some(&field_strings), Some(input));

        if let Ok(body) = serde_json::from_str::<CreateWebhookRequest>(input) {
            exercise_normalizers(
                &body.events,
                body.modified_fields.as_ref(),
                body.name.as_deref(),
            );
        }

        if let Ok(body) = serde_json::from_str::<UpdateWebhookRequest>(input) {
            exercise_normalizers(
                &body.events,
                body.modified_fields.as_ref(),
                body.name.as_deref(),
            );
        }
    }
});
