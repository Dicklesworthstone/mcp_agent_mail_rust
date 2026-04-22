#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mcp_agent_mail_storage::{WbqEnqueueResult, fuzz_wbq_enqueue_scenario};

#[derive(Arbitrary, Debug)]
struct WbqInput {
    capacity: u8,
    prefill: u8,
    drop_receiver: bool,
    disk_pressure_level: u64,
}

fuzz_target!(|input: WbqInput| {
    let outcome = fuzz_wbq_enqueue_scenario(
        input.capacity,
        input.prefill,
        input.drop_receiver,
        input.disk_pressure_level,
    );

    match outcome.result {
        WbqEnqueueResult::Enqueued => {
            assert_eq!(
                outcome.depth, 1,
                "successful enqueue must account for exactly one accepted op"
            );
            assert!(
                outcome.remaining_messages >= 1,
                "successful enqueue should leave a message for the drain worker"
            );
        }
        WbqEnqueueResult::QueueUnavailable | WbqEnqueueResult::SkippedDiskCritical => {
            assert_eq!(
                outcome.depth, 0,
                "rejected enqueue must not increment queue depth"
            );
        }
    }
});
