#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mcp_agent_mail_guard::{
    FileReservationRecord, fuzz_check_path_conflicts, fuzz_contains_glob, fuzz_normalize_path,
};

#[derive(Arbitrary, Debug)]
struct ReservationInput {
    pattern: String,
    holder: String,
    exclusive: bool,
}

#[derive(Arbitrary, Debug)]
struct ConflictInput {
    paths: Vec<String>,
    reservations: Vec<ReservationInput>,
    self_agent: String,
    ignorecase: bool,
}

fn reservation(input: &ReservationInput, ignorecase: bool) -> FileReservationRecord {
    FileReservationRecord {
        path_pattern: input.pattern.clone(),
        agent_name: input.holder.clone(),
        exclusive: input.exclusive,
        expires_ts: "2099-01-01T00:00:00Z".to_string(),
        released_ts: None,
        normalized_pattern: fuzz_normalize_path(&input.pattern, ignorecase),
        has_glob: fuzz_contains_glob(&input.pattern),
    }
}

fn concrete_path(path: &str) -> bool {
    !path
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
}

fuzz_target!(|input: ConflictInput| {
    let reservations: Vec<FileReservationRecord> = input
        .reservations
        .iter()
        .take(64)
        .map(|item| reservation(item, input.ignorecase))
        .collect();
    let paths: Vec<String> = input.paths.into_iter().take(64).collect();

    let Ok(conflicts) =
        fuzz_check_path_conflicts(&paths, &reservations, &input.self_agent, input.ignorecase)
    else {
        return;
    };

    for conflict in &conflicts {
        assert!(
            paths.iter().any(|path| path == &conflict.path),
            "conflict path must come from the checked path set"
        );
        assert!(
            reservations
                .iter()
                .any(|res| res.path_pattern == conflict.pattern
                    && res.agent_name == conflict.holder
                    && res.exclusive
                    && !res.agent_name.eq_ignore_ascii_case(&input.self_agent)),
            "conflict must point at an active reservation held by another agent"
        );
    }

    for path in &paths {
        let normalized = fuzz_normalize_path(path, input.ignorecase);
        if normalized.is_empty() || path.contains('\0') || !concrete_path(path) {
            continue;
        }
        let other_agent = if input.self_agent.eq_ignore_ascii_case("fuzz-other-agent") {
            "fuzz-different-agent"
        } else {
            "fuzz-other-agent"
        };

        let self_owned = FileReservationRecord {
            path_pattern: path.clone(),
            agent_name: input.self_agent.clone(),
            exclusive: true,
            expires_ts: "2099-01-01T00:00:00Z".to_string(),
            released_ts: None,
            normalized_pattern: fuzz_normalize_path(path, input.ignorecase),
            has_glob: fuzz_contains_glob(path),
        };
        let self_conflicts = fuzz_check_path_conflicts(
            std::slice::from_ref(path),
            &[self_owned],
            &input.self_agent,
            input.ignorecase,
        )
        .expect("self-owned literal reservation check should not fail");
        assert!(
            self_conflicts.is_empty(),
            "self-owned reservations must never block their holder"
        );

        let other_owned = FileReservationRecord {
            path_pattern: path.clone(),
            agent_name: other_agent.to_string(),
            exclusive: true,
            expires_ts: "2099-01-01T00:00:00Z".to_string(),
            released_ts: None,
            normalized_pattern: fuzz_normalize_path(path, input.ignorecase),
            has_glob: fuzz_contains_glob(path),
        };
        let other_conflicts = fuzz_check_path_conflicts(
            std::slice::from_ref(path),
            &[other_owned],
            &input.self_agent,
            input.ignorecase,
        )
        .expect("other-owned literal reservation check should not fail");
        assert_eq!(
            other_conflicts.len(),
            1,
            "other-owned literal reservation should block the exact concrete path"
        );
    }
});
