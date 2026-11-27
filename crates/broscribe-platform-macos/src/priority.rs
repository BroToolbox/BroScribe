//! Darwin QoS at the thread and task boundaries; no real-time scheduling or
//! privileged negative nice values are required.

use broscribe_platform::{Error, PriorityClass, ProcessPriority, Result};

#[derive(Default)]
pub struct MacProcessPriority;

// Public SDK declarations in <pthread/qos.h> and <mach/task_policy.h>.
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
    fn task_policy_set(task: u32, flavor: u32, policy: *const i32, count: u32) -> i32;
}

fn thread_qos(class: PriorityClass) -> u32 {
    match class {
        PriorityClass::Capture => 0x21,     // QOS_CLASS_USER_INTERACTIVE
        PriorityClass::Interactive => 0x19, // QOS_CLASS_USER_INITIATED
        PriorityClass::Utility => 0x11,     // QOS_CLASS_UTILITY
        PriorityClass::Background => 0x09,  // QOS_CLASS_BACKGROUND
    }
}

fn task_qos(class: PriorityClass) -> [i32; 2] {
    // TASK_BASE_QOS_POLICY: latency and throughput tiers, not pthread QoS enums.
    let tier = match class {
        PriorityClass::Capture => 1,
        PriorityClass::Interactive => 2,
        PriorityClass::Utility => 4,
        PriorityClass::Background => 6,
    };
    [(0xff << 16) | tier, (0xfe << 16) | tier]
}

impl ProcessPriority for MacProcessPriority {
    fn set_current_thread(&self, class: PriorityClass) -> Result<()> {
        // SAFETY: a documented QoS class and valid relative priority (zero).
        let status = unsafe { pthread_set_qos_class_self_np(thread_qos(class), 0) };
        if status != 0 {
            return Err(Error::io(
                "pthread_set_qos_class_self_np",
                std::io::Error::from_raw_os_error(status),
            ));
        }
        Ok(())
    }

    fn set_current_process(&self, class: PriorityClass) -> Result<()> {
        let policy = task_qos(class);
        // SAFETY: the send right is this process's task; the policy contains
        // exactly TASK_QOS_POLICY_COUNT integer_t values, valid for flavor 8.
        #[allow(deprecated)]
        // libc exposes this task port; no additional Mach dependency is needed.
        let status = unsafe { task_policy_set(libc::mach_task_self(), 8, policy.as_ptr(), 2) };
        if status != 0 {
            return Err(Error::Internal(format!(
                "task_policy_set(TASK_BASE_QOS_POLICY): Mach error {status}"
            )));
        }
        // Classify the initial thread too; task policy covers process-wide
        // latency/throughput, while pthread QoS supplies the CPU work class.
        self.set_current_thread(class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::priority::{FakeProcessPriority, PriorityCall};

    #[test]
    fn classes_map_in_priority_order() {
        let classes = [
            PriorityClass::Capture,
            PriorityClass::Interactive,
            PriorityClass::Utility,
            PriorityClass::Background,
        ];
        for pair in classes.windows(2) {
            assert!(thread_qos(pair[0]) > thread_qos(pair[1]));
            assert!(task_qos(pair[0])[0] < task_qos(pair[1])[0]);
        }
        let fake = FakeProcessPriority::default();
        fake.set_current_thread(PriorityClass::Capture).unwrap();
        fake.set_current_process(PriorityClass::Utility).unwrap();
        assert_eq!(
            fake.calls(),
            [
                PriorityCall::Thread(PriorityClass::Capture),
                PriorityCall::Process(PriorityClass::Utility)
            ]
        );
    }

    #[test]
    #[ignore = "changes native QoS; run explicitly on macOS"]
    fn native_thread_classes() {
        std::thread::spawn(|| {
            for class in [
                PriorityClass::Capture,
                PriorityClass::Interactive,
                PriorityClass::Utility,
                PriorityClass::Background,
            ] {
                MacProcessPriority.set_current_thread(class).unwrap();
            }
        })
        .join()
        .unwrap();
    }
}
