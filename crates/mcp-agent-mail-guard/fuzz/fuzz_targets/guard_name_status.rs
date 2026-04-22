#![no_main]

use libfuzzer_sys::fuzz_target;
use mcp_agent_mail_guard::fuzz_parse_name_status_z;

fuzz_target!(|raw: &[u8]| {
    let Ok(paths) = fuzz_parse_name_status_z(raw) else {
        return;
    };

    for path in paths {
        assert!(
            !path.is_empty(),
            "name-status parser should not emit empty path entries"
        );
        assert!(
            !path.contains('\0'),
            "name-status parser should split NUL delimiters out of paths"
        );
    }
});
