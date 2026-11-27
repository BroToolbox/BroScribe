use broscribe_ipc::{AgentPing, IpcError, PROTOCOL_VERSION};
use serde_json::Value;

#[test]
fn ping_rejects_legacy_missing_or_inconsistent_runtime_identity() {
    let ready = serde_json::to_value(AgentPing::maintenance(None)).unwrap();
    AgentPing::from_value(ready.clone()).unwrap();

    let mut legacy = ready.clone();
    legacy.as_object_mut().unwrap().remove("mode");
    legacy.as_object_mut().unwrap().remove("readiness");
    assert_eq!(
        AgentPing::from_value(legacy).unwrap_err().code,
        IpcError::PROTOCOL
    );

    let mut inconsistent = ready.clone();
    inconsistent["readiness"] = Value::from("normal_operational");
    assert_eq!(
        AgentPing::from_value(inconsistent).unwrap_err().code,
        IpcError::PROTOCOL
    );

    let mut old = ready.clone();
    old["protocol"] = Value::from(PROTOCOL_VERSION - 1);
    assert_eq!(
        AgentPing::from_value(old).unwrap_err().code,
        IpcError::UNSUPPORTED_VERSION
    );

    let mut missing_process = ready;
    missing_process["pid"] = Value::from(0);
    assert_eq!(
        AgentPing::from_value(missing_process).unwrap_err().code,
        IpcError::PROTOCOL
    );
}
