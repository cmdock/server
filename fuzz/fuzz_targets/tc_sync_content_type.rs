#![no_main]

use libfuzzer_sys::fuzz_target;

const HISTORY_SEGMENT_CONTENT_TYPE: &str = "application/vnd.taskchampion.history-segment";
const SNAPSHOT_CONTENT_TYPE: &str = "application/vnd.taskchampion.snapshot";

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = cmdock_server::tc_sync::handlers::content_type_matches(
            input,
            HISTORY_SEGMENT_CONTENT_TYPE,
        );
        let _ =
            cmdock_server::tc_sync::handlers::content_type_matches(input, SNAPSHOT_CONTENT_TYPE);
    }
});
