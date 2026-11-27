use broscribe_core::Error;
use broscribe_core::maintenance::{BUSY_CODE, ERROR_CODE};
use broscribe_ipc::IpcError;

#[test]
fn maintenance_errors_keep_their_code_through_ipc() {
    for code in [ERROR_CODE, BUSY_CODE] {
        let error = Error::Service {
            code,
            message: "synthetic maintenance refusal".into(),
        };
        let received = IpcError::from(error).into_core();
        assert_eq!(received.code(), code);
    }
}
